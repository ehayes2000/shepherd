use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, bounded};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use vt100::{Callbacks, Parser, Screen};

/// Type alias for parser with terminal callbacks
type CallbackParser = Parser<TerminalCallbacks>;

/// Shared writer for sending responses back to the PTY
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Terminal callbacks that respond to escape sequence queries
pub struct TerminalCallbacks {
    writer: SharedWriter,
}

impl TerminalCallbacks {
    pub fn new(writer: SharedWriter) -> Self {
        Self { writer }
    }

    fn write_response(&mut self, response: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(response);
            let _ = writer.flush();
        }
    }
}

// TODO: this is incomplete + likely wrong
impl Callbacks for TerminalCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, c) {
            // CSI 5 n - Device Status Report (operating status)
            // Response: CSI 0 n (terminal OK)
            (None, 'n') if params == [[5]] => {
                self.write_response(b"\x1b[0n");
            }
            // CSI 6 n - Device Status Report (cursor position)
            // Response: CSI row ; col R
            (None, 'n') if params == [[6]] => {
                let (row, col) = screen.cursor_position();
                let response = format!("\x1b[{};{}R", row + 1, col + 1);
                self.write_response(response.as_bytes());
            }
            // CSI c or CSI 0 c - Primary Device Attributes (DA1)
            // Response: VT220 with various capabilities
            (None, 'c') if params.is_empty() || params == [[0]] => {
                // Report as VT220 with ANSI color, etc.
                self.write_response(b"\x1b[?62;1;2;6;22c");
            }
            // CSI > c or CSI > 0 c - Secondary Device Attributes (DA2)
            (Some(b'>'), 'c') if params.is_empty() || params == [[0]] => {
                // Terminal type 0, version 0, ROM version 0
                self.write_response(b"\x1b[>0;0;0c");
            }
            // CSI ? 6 n - DECXCPR (extended cursor position)
            (Some(b'?'), 'n') if params == [[6]] => {
                let (row, col) = screen.cursor_position();
                let response = format!("\x1b[?{};{}R", row + 1, col + 1);
                self.write_response(response.as_bytes());
            }
            _ => {}
        }
    }
}

const SCROLLBACK: usize = 1000;
const BUF_SIZE: usize = 8 * 1024;

/// (rows, cols) ordered size stored in AtomicU32
#[derive(Clone, Debug)]
pub struct SharedSize(Arc<AtomicU32>);

impl SharedSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self(Arc::new(AtomicU32::new(Self::pack_size(rows, cols))))
    }

    fn pack_size(rows: u16, cols: u16) -> u32 {
        ((rows as u32) << 16) | cols as u32
    }

    pub fn get(&self) -> (u16, u16) {
        let inner = self.0.load(Ordering::Relaxed);
        let rows = (inner >> 16) as u16;
        let cols = (inner & 0xFFFF) as u16;
        (rows, cols)
    }

    pub fn set(&self, rows: u16, cols: u16) {
        self.0.store(Self::pack_size(rows, cols), Ordering::Relaxed);
    }
}

pub struct Session {
    active: Arc<AtomicBool>,
    writer: SharedWriter,
    _reader_thread: JoinHandle<()>,
    /// Shared parser - owned by reader thread but accessible for on-demand screen cloning
    parser: Arc<Mutex<CallbackParser>>,
    /// Cached screen clone for rendering (only updated when dirty)
    cached_screen: ArcSwap<Screen>,
    /// Dirty flag - set by reader thread, cleared when screen is cloned
    dirty: Arc<AtomicBool>,
    /// Channel to signal the reader thread to shut down
    shutdown_tx: Sender<()>,
    /// Error message if the session died unexpectedly
    session_error: Arc<ArcSwap<Option<String>>>,
    /// Child process handle for killing
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
}

impl Session {
    /// Check if the session has died (reader thread encountered an error)
    pub fn is_dead(&self) -> bool {
        self.session_error.load().is_some()
    }

    /// Get the error message if the session died
    pub fn get_error(&self) -> Option<String> {
        self.session_error.load().as_ref().clone()
    }

    /// Signal the reader thread to shut down gracefully and kill the child process
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.try_send(());
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// Get the current screen state (clones only if dirty)
    pub fn get_screen(&self) -> Arc<Screen> {
        // Only clone the screen if it's been modified since last read
        if self.dirty.swap(false, Ordering::AcqRel)
            && let Ok(parser) = self.parser.lock()
        {
            self.cached_screen.store(Arc::new(parser.screen().clone()));
        }
        self.cached_screen.load_full()
    }
}

pub struct DetachedSession(Session);

impl Deref for DetachedSession {
    type Target = Session;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DetachedSession {
    pub fn attach(self) -> anyhow::Result<AttachedSession> {
        self.0.active.store(true, Ordering::Release);
        Ok(AttachedSession(self.0))
    }
}

/// A session that is attached to the terminal - user can interact with it
pub struct AttachedSession(pub Session);

impl Deref for AttachedSession {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for AttachedSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AttachedSession {
    pub fn new_with_env(
        command: &str,
        args: &[&str],
        _tx: std::sync::mpsc::Sender<Screen>,
        size: SharedSize,
        cwd: Option<&Path>,
        env_vars: &[(&str, &str)],
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();

        let (rows, cols) = size.get();
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size)?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let child = Arc::new(Mutex::new(child));

        let mut reader = pair.master.try_clone_reader()?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let callback_writer = writer.clone();

        let active = Arc::new(AtomicBool::new(true));
        let shared_active = active.clone();

        // Create parser with callbacks - shared between reader thread and main thread
        let callbacks = TerminalCallbacks::new(callback_writer);
        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            rows, cols, SCROLLBACK, callbacks,
        )));
        let shared_parser = parser.clone();

        // Create initial cached screen
        let initial_screen = parser.lock().unwrap().screen().clone();
        let cached_screen = ArcSwap::from_pointee(initial_screen);

        // Dirty flag - starts false since cached_screen is in sync
        let dirty = Arc::new(AtomicBool::new(false));
        let shared_dirty = dirty.clone();

        // Create shutdown channel - bounded(1) for non-blocking send
        let (shutdown_tx, shutdown_rx): (Sender<()>, Receiver<()>) = bounded(1);

        // Create error reporting channel
        let session_error: Arc<ArcSwap<Option<String>>> = Arc::new(ArcSwap::from_pointee(None));
        let shared_error = session_error.clone();

        let reader_thread = std::thread::spawn(move || {
            let master = pair.master;
            let mut buf = [0u8; BUF_SIZE];
            loop {
                // Check for shutdown signal (non-blocking)
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF - child process exited
                        shared_error.store(Arc::new(Some("Process exited".to_string())));
                        break;
                    }
                    Ok(n) => {
                        // Check if size changed and update both PTY and parser
                        let (rows, cols) = size.get();

                        // Handle PTY size query gracefully
                        let current = match master.get_size() {
                            Ok(size) => size,
                            Err(e) => {
                                shared_error.store(Arc::new(Some(format!(
                                    "PTY error: failed to get size: {}",
                                    e
                                ))));
                                break;
                            }
                        };

                        if current.rows != rows || current.cols != cols {
                            // Handle PTY resize gracefully
                            if let Err(e) = master.resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            }) {
                                shared_error.store(Arc::new(Some(format!(
                                    "PTY error: failed to resize: {}",
                                    e
                                ))));
                                break;
                            }
                        }

                        // Lock parser, process data, set dirty flag
                        // No screen cloning here - that happens on-demand in get_screen()
                        if let Ok(mut parser) = shared_parser.lock() {
                            parser.screen_mut().set_size(rows, cols);
                            parser.process(&buf[..n]);
                        }
                        shared_dirty.store(true, Ordering::Release);

                        let is_active = shared_active.load(Ordering::Acquire);
                        if !is_active {
                            continue;
                        }
                    }
                    Err(e) => {
                        // Read error - PTY closed or child died
                        let kind = e.kind();
                        // EIO is expected when child process exits
                        if kind != std::io::ErrorKind::Other {
                            shared_error.store(Arc::new(Some(format!("PTY read error: {}", e))));
                        } else {
                            // EIO means process exited
                            shared_error.store(Arc::new(Some("Process exited".to_string())));
                        }
                        break;
                    }
                }
            }
        });

        Ok(Self(Session {
            active,
            writer,
            _reader_thread: reader_thread,
            parser,
            cached_screen,
            dirty,
            shutdown_tx,
            session_error,
            child,
        }))
    }

    pub fn detach(self) -> DetachedSession {
        self.0.active.store(false, Ordering::Release);
        DetachedSession(self.0)
    }

    pub fn write_input(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }
}
