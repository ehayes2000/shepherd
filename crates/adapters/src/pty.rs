//! `Pty` on top of portable-pty (unix).
//!
//! Each session has one pump thread, and it alone reaps and signals the child, so a pid is never signalled
//! after it has been reaped and possibly reused. The pump forwards output while polling for the child's exit.
//! When the child exits, the pump drains what is left on the master and closes the slave. Then it forgets the
//! session and sends `Exited`. The parent holds a slave handle until the drain finishes, because on macOS
//! closing the last slave handle throws away output that hasn't been read yet.
//!
//! The pump counts the output it has sent and not had acknowledged. At [`OUTPUT_CREDIT`] it stops reading (but
//! keeps checking for exit and kill) until an ack arrives, so the child blocks once the kernel's buffer is full.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, SlavePty, native_pty_system};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec,
};

/// How often the pump checks for exit and kill requests while the child is quiet.
const TICK: Duration = Duration::from_millis(20);
/// How long `kill` waits after SIGHUP before sending SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// What callers touch. Each part has its own lock, so a stuck write to one session blocks only that session.
struct Live {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    control: Sender<Control>,
}

/// What callers tell a session's pump.
enum Control {
    Kill,
    Ack(usize),
}

struct Entry {
    live: Arc<Live>,
    pump: JoinHandle<()>,
}

type Sessions = Arc<Mutex<HashMap<SessionId, Entry>>>;

pub struct PortablePty {
    sink: Arc<dyn PtyEventSink>,
    sessions: Sessions,
}

impl PortablePty {
    pub fn new(sink: Arc<dyn PtyEventSink>) -> Self {
        Self {
            sink,
            sessions: Arc::default(),
        }
    }

    fn live(&self, session: &SessionId) -> Result<Arc<Live>, PtyError> {
        lock(&self.sessions)
            .get(session)
            .map(|entry| entry.live.clone())
            .ok_or(PtyError::UnknownSession)
    }
}

impl Pty for PortablePty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError> {
        // Held until the entry is inserted, so the pump can't forget the session before it exists.
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&session) {
            return Err(PtyError::AlreadyExists);
        }

        let pair = native_pty_system()
            .openpty(pty_size(spec.size))
            .map_err(io_error)?;
        let mut cmd = CommandBuilder::new(&spec.command);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::SpawnFailed(e.to_string()))?;

        let master = pair.master;
        let handles = (|| {
            let fd = master
                .as_raw_fd()
                .ok_or_else(|| io_error("pty master has no fd"))?;
            let reader = master.try_clone_reader().map_err(io_error)?;
            let writer = master.take_writer().map_err(io_error)?;
            Ok((fd, reader, writer))
        })();
        let (fd, reader, writer) = match handles {
            Ok(handles) => handles,
            Err(e) => {
                abandon(child.as_mut());
                return Err(e);
            }
        };

        let (control, controls) = mpsc::channel();
        let live = Arc::new(Live {
            master: Mutex::new(master),
            writer: Mutex::new(writer),
            control,
        });
        let pump = Pump {
            session: session.clone(),
            live: live.clone(),
            fd,
            reader,
            slave: pair.slave,
            controls,
            unacked: 0,
            sessions: self.sessions.clone(),
            sink: self.sink.clone(),
        };
        let pid = child.process_id();
        let pump = thread::Builder::new()
            .name(format!("pty-{session}"))
            .spawn(move || pump.run(child))
            .map_err(|e| {
                // Nobody will reap it now, but at least it won't keep running.
                signal_group(pid, libc::SIGKILL);
                io_error(e)
            })?;

        sessions.insert(session, Entry { live, pump });
        Ok(())
    }

    fn write(&mut self, session: &SessionId, bytes: &[u8]) -> Result<(), PtyError> {
        let live = self.live(session)?;
        let mut writer = lock(&live.writer);
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(io_error)
    }

    fn resize(&mut self, session: &SessionId, size: PtySize) -> Result<(), PtyError> {
        let live = self.live(session)?;
        let master = lock(&live.master);
        master.resize(pty_size(size)).map_err(io_error)
    }

    fn kill(&mut self, session: &SessionId) -> Result<(), PtyError> {
        let live = self.live(session)?;
        // The pump only stops listening after it has forgotten the session.
        live.control
            .send(Control::Kill)
            .map_err(|_| PtyError::UnknownSession)
    }

    fn ack(&mut self, session: &SessionId, bytes: usize) -> Result<(), PtyError> {
        let live = self.live(session)?;
        live.control
            .send(Control::Ack(bytes))
            .map_err(|_| PtyError::UnknownSession)
    }
}

impl Drop for PortablePty {
    /// Kills every live session and waits for its pump to finish, so no process or thread outlives the port.
    /// Each session still sends its `Exited`.
    fn drop(&mut self) {
        let entries: Vec<Entry> = lock(&self.sessions).drain().map(|(_, e)| e).collect();
        for entry in &entries {
            let _ = entry.live.control.send(Control::Kill);
        }
        for entry in entries {
            let _ = entry.pump.join();
        }
    }
}

struct Pump {
    session: SessionId,
    /// Keeps the master (and so `fd`) open for as long as the pump polls it.
    live: Arc<Live>,
    fd: RawFd,
    reader: Box<dyn Read + Send>,
    slave: Box<dyn SlavePty + Send>,
    controls: Receiver<Control>,
    /// Output sent and not acknowledged yet.
    unacked: usize,
    sessions: Sessions,
    sink: Arc<dyn PtyEventSink>,
}

enum Killing {
    No,
    HungUp { at: Instant },
    Killed,
}

impl Pump {
    fn run(mut self, mut child: Box<dyn Child + Send + Sync>) {
        let pid = child.process_id();
        let mut output_open = true;
        let mut killing = Killing::No;

        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break exit_code(&status),
                Ok(None) => {}
                Err(_) => break None,
            }

            let mut kill_requested = false;
            let killed = !matches!(killing, Killing::No);
            // A dying child may be stuck in `exit` until its output is read (macOS drains the tty on close), so
            // once it is being killed it is read whatever the credit.
            if output_open && (self.unacked < OUTPUT_CREDIT || killed) {
                if readable(self.fd, TICK) {
                    output_open = self.forward(killed);
                }
            } else if let Ok(control) = self.controls.recv_timeout(TICK) {
                kill_requested |= self.apply(control);
            }
            while let Ok(control) = self.controls.try_recv() {
                kill_requested |= self.apply(control);
            }

            killing = match killing {
                Killing::No if kill_requested => {
                    signal_group(pid, libc::SIGHUP);
                    Killing::HungUp { at: Instant::now() }
                }
                Killing::HungUp { at } if at.elapsed() >= KILL_GRACE => {
                    signal_group(pid, libc::SIGKILL);
                    Killing::Killed
                }
                other => other,
            };
        };

        // The child is gone but its output may still be queued on the master. That much is bounded by the kernel's
        // buffer, so it is read whatever the credit, and `Exited` never waits for an ack.
        let killed = !matches!(killing, Killing::No);
        while output_open && readable(self.fd, Duration::ZERO) {
            output_open = self.forward(killed);
        }
        drop(self.slave);
        drop(self.live);

        lock(&self.sessions).remove(&self.session);
        self.sink.send(PtyEvent::Exited {
            session: self.session,
            code,
        });
    }

    /// Takes in one control message. Returns whether it asks for a kill.
    fn apply(&mut self, control: Control) -> bool {
        match control {
            Control::Kill => true,
            Control::Ack(n) => {
                self.unacked = self.unacked.saturating_sub(n);
                false
            }
        }
    }

    /// Reads once and forwards it. A `killed` session's output past its credit is read and dropped, so a kill never
    /// waits on an ack and a flooding child that ignores SIGHUP can't run past the credit during the grace period.
    /// Returns false once the master has nothing more to give.
    fn forward(&mut self, killed: bool) -> bool {
        let mut buf = [0u8; MAX_OUTPUT_CHUNK];
        match self.reader.read(&mut buf) {
            Ok(0) => false,
            Ok(_) if killed && self.unacked >= OUTPUT_CREDIT => true,
            Ok(n) => {
                self.unacked += n;
                self.sink.send(PtyEvent::Output {
                    session: self.session.clone(),
                    bytes: buf[..n].to_vec(),
                });
                true
            }
            Err(e) => matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock),
        }
    }
}

/// Waits up to `timeout` for `fd` to have something to read (or to hang up).
fn readable(fd: RawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid pollfd, and the fd is kept open by the caller.
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

/// The child is a session leader (portable-pty calls `setsid`), so its pid is its process group.
fn signal_group(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: plain syscall. Only the pump calls it, and only before reaping, so the group still exists.
        unsafe { libc::killpg(pid, signal) };
    }
}

/// Kills and reaps a child we failed to finish setting up.
fn abandon(child: &mut (dyn Child + Send + Sync)) {
    let _ = child.kill();
    let _ = child.wait();
}

fn exit_code(status: &ExitStatus) -> Option<i32> {
    match status.signal() {
        Some(_) => None,
        None => i32::try_from(status.exit_code()).ok(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn pty_size(size: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn io_error(e: impl std::fmt::Display) -> PtyError {
    PtyError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;

    use shepherd_core::ids::SessionId;
    use shepherd_core::ports::{Pty, PtyEvent};
    use shepherd_testkit::contracts::pty::{ChannelSink, spec};

    use super::PortablePty;

    shepherd_testkit::pty_contract!(PortablePty::new);

    #[test]
    fn drop_kills_sessions_and_waits_for_their_exit() {
        let (tx, rx) = mpsc::channel();
        let mut pty = PortablePty::new(Arc::new(ChannelSink(tx)));
        let s = SessionId::new("stubborn");
        pty.spawn(s.clone(), spec("sh", &["-c", "trap '' HUP; cat"]))
            .unwrap();

        drop(pty);

        // No waiting: by the time drop returns, the pump has finished and sent Exited.
        let exits: Vec<_> = rx
            .try_iter()
            .filter(|e| matches!(e, PtyEvent::Exited { .. }))
            .collect();
        assert_eq!(
            exits,
            vec![PtyEvent::Exited {
                session: s,
                code: None
            }]
        );
    }
}
