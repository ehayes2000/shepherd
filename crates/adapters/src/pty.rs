//! `Pty` on top of portable-pty (unix), and `PtyHandoff` on raw file descriptors.
//!
//! Each session has one pump thread, and it alone reaps and signals the child, so a pid is never signalled
//! after it has been reaped and possibly reused. The pump forwards output while polling for the child's exit.
//! When the child exits, the pump drains what is left on the master and closes the slave. Then it forgets the
//! session and sends `Exited`. The parent holds a slave handle until the drain finishes, because on macOS
//! closing the last slave handle throws away output that hasn't been read yet.
//!
//! The pump counts the output it has sent and not had acknowledged. At [`OUTPUT_CREDIT`] it stops reading (but
//! keeps checking for exit and kill) until an ack arrives, so the child blocks once the kernel's buffer is full.
//!
//! Handoff (decision 0018). [`PtyHandoff::export`] pauses every pump: a paused pump has forwarded everything it read,
//! and reads, reaps and signals nothing until it is resumed or released. Each exported session's [`PtyToken`] holds
//! the child's pid and fresh descriptors of its master and slave (`CLOEXEC`, owned by whoever adopts them; the
//! exporter closes them only on [`PtyHandoff::resume`]). The host re-execs itself with them, and the new image adopts
//! them: portable-pty can't wrap an existing descriptor, so an adopted session reads, writes and resizes the master
//! fd directly (`TIOCSWINSZ`) and reaps the child with `waitpid`. That works because exec keeps the pid: the agent is
//! still our child. A dropped exporter releases its exported pumps without touching their sessions.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, native_pty_system};
use serde::{Deserialize, Serialize};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtyHandoff, PtySize,
    SpawnSpec,
};

/// How often the pump checks for exit and kill requests while the child is quiet.
const TICK: Duration = Duration::from_millis(20);
/// How long `kill` waits after SIGHUP before sending SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// What callers touch. Each part has its own lock, so a stuck write to one session blocks only that session.
struct Live {
    master: Mutex<Box<dyn Master>>,
    writer: Mutex<Box<dyn Write + Send>>,
    control: Sender<Control>,
    /// Where an export gets the session's slave from.
    slave: SlaveSource,
}

/// What callers tell a session's pump.
enum Control {
    Kill,
    Ack(usize),
    /// Stop reading and reaping, then answer with the child's pid once everything read so far is sent.
    Pause(Sender<Option<u32>>),
    /// Carry on after a pause.
    Resume,
    /// After a pause: the session is someone else's now. Stop without touching it.
    Release,
}

/// A session's master, spawned by portable-pty or adopted as a raw fd.
trait Master: Send {
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;
    fn fd(&self) -> Option<RawFd>;
}

impl Master for Box<dyn MasterPty + Send> {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        MasterPty::resize(&**self, pty_size(size)).map_err(io_error)
    }

    fn fd(&self) -> Option<RawFd> {
        self.as_raw_fd()
    }
}

/// An adopted session's master.
struct RawMaster(OwnedFd);

impl Master for RawMaster {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let winsize = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: a valid winsize for an fd we own.
        let rc = unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
        if rc == -1 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn fd(&self) -> Option<RawFd> {
        Some(self.0.as_raw_fd())
    }
}

/// How to get a descriptor of a session's slave for an export.
enum SlaveSource {
    /// A spawned session's slave, by name (portable-pty doesn't expose its fd).
    Path(PathBuf),
    /// An adopted session's slave, held open here until its pump is done.
    Fd(OwnedFd),
}

impl SlaveSource {
    fn open(&self) -> std::io::Result<OwnedFd> {
        match self {
            Self::Path(path) => {
                // O_NOCTTY: reopening a slave must never make it our controlling terminal.
                let opened = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOCTTY)
                    .open(path)?;
                Ok(OwnedFd::from(opened))
            }
            Self::Fd(fd) => fd.try_clone(),
        }
    }
}

/// A session's child, spawned by portable-pty or adopted by pid.
trait Proc: Send {
    /// `Some(code)` once it has exited (`code` is `None` if a signal ended it), reaping it.
    fn try_wait(&mut self) -> std::io::Result<Option<Option<i32>>>;
    fn pid(&self) -> Option<u32>;
}

impl Proc for Box<dyn Child + Send + Sync> {
    fn try_wait(&mut self) -> std::io::Result<Option<Option<i32>>> {
        Ok(
            Child::try_wait(&mut **self)?.map(|status| match status.signal() {
                Some(_) => None,
                None => i32::try_from(status.exit_code()).ok(),
            }),
        )
    }

    fn pid(&self) -> Option<u32> {
        self.process_id()
    }
}

/// An adopted child: still ours after the exec, so `waitpid` reaps it.
struct RawChild(libc::pid_t);

impl Proc for RawChild {
    fn try_wait(&mut self) -> std::io::Result<Option<Option<i32>>> {
        let mut status = 0;
        // SAFETY: plain syscall on our own child.
        match unsafe { libc::waitpid(self.0, &mut status, libc::WNOHANG) } {
            0 => Ok(None),
            -1 => Err(std::io::Error::last_os_error()),
            _ if libc::WIFEXITED(status) => Ok(Some(Some(libc::WEXITSTATUS(status)))),
            _ => Ok(Some(None)),
        }
    }

    fn pid(&self) -> Option<u32> {
        u32::try_from(self.0).ok()
    }
}

struct Entry {
    live: Arc<Live>,
    pump: JoinHandle<()>,
}

/// An exported session: its paused pump, and the descriptors its token carries.
struct Exported {
    entry: Entry,
    carried: [RawFd; 2],
}

type Sessions = Arc<Mutex<HashMap<SessionId, Entry>>>;

/// What carries one session across the host's exec: its child's pid, and descriptors of its master and slave.
/// Plain numbers, written into the handoff file; the descriptors are open (and `CLOEXEC`) until adopted or resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyToken {
    pub pid: u32,
    pub master: RawFd,
    pub slave: RawFd,
}

impl PtyToken {
    /// The descriptors the exec must carry.
    pub fn fds(&self) -> [RawFd; 2] {
        [self.master, self.slave]
    }
}

pub struct PortablePty {
    sink: Arc<dyn PtyEventSink>,
    sessions: Sessions,
    exported: HashMap<SessionId, Exported>,
}

impl PortablePty {
    pub fn new(sink: Arc<dyn PtyEventSink>) -> Self {
        Self {
            sink,
            sessions: Arc::default(),
            exported: HashMap::new(),
        }
    }

    fn live(&self, session: &SessionId) -> Result<Arc<Live>, PtyError> {
        lock(&self.sessions)
            .get(session)
            .map(|entry| entry.live.clone())
            .ok_or(PtyError::UnknownSession)
    }

    /// Starts `pump` on its own thread and records the session. Kills `pid` if no thread can be started.
    fn start_pump(
        &self,
        sessions: &mut HashMap<SessionId, Entry>,
        session: SessionId,
        pump: Pump,
        child: Box<dyn Proc>,
    ) -> Result<(), PtyError> {
        let pid = child.pid();
        let live = pump.live.clone();
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

    /// Pauses every live pump. Returns each paused session with its child's pid. A pump that is past its child's
    /// exit never answers the pause; it finishes as usual and isn't returned.
    fn pause_all(&self) -> Vec<(SessionId, Option<u32>)> {
        let live: Vec<(SessionId, Arc<Live>)> = lock(&self.sessions)
            .iter()
            .map(|(id, e)| (id.clone(), e.live.clone()))
            .collect();
        let mut paused = Vec::new();
        for (id, live) in live {
            let (tx, rx) = mpsc::channel();
            if live.control.send(Control::Pause(tx)).is_err() {
                continue;
            }
            // Disconnected: the pump finished without reading the request.
            if let Ok(pid) = rx.recv() {
                paused.push((id, pid));
            }
        }
        paused
    }

    /// Fresh descriptors of `session`'s master and slave, for its token.
    fn carried_fds(&self, session: &SessionId) -> std::io::Result<(OwnedFd, OwnedFd)> {
        let live = self
            .live(session)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let master = {
            let master = lock(&live.master);
            let fd = master
                .fd()
                .ok_or_else(|| std::io::Error::other("pty master has no fd"))?;
            dup_cloexec(fd)?
        };
        let slave = live.slave.open()?;
        Ok((master, slave))
    }
}

impl Pty for PortablePty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError> {
        // Held until the entry is inserted, so the pump can't forget the session before it exists.
        let sessions_handle = self.sessions.clone();
        let mut sessions = lock(&sessions_handle);
        if sessions.contains_key(&session) || self.exported.contains_key(&session) {
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
            let tty = master
                .tty_name()
                .ok_or_else(|| io_error("pty slave has no name"))?;
            let reader = master.try_clone_reader().map_err(io_error)?;
            let writer = master.take_writer().map_err(io_error)?;
            Ok((fd, tty, reader, writer))
        })();
        let (fd, tty, reader, writer) = match handles {
            Ok(handles) => handles,
            Err(e) => {
                abandon(child.as_mut());
                return Err(e);
            }
        };

        let (control, controls) = mpsc::channel();
        let live = Arc::new(Live {
            master: Mutex::new(Box::new(master)),
            writer: Mutex::new(writer),
            control,
            slave: SlaveSource::Path(tty),
        });
        let pump = Pump {
            session: session.clone(),
            live,
            fd,
            reader,
            slave: Box::new(pair.slave),
            controls,
            unacked: 0,
            sessions: self.sessions.clone(),
            sink: self.sink.clone(),
        };
        self.start_pump(&mut sessions, session, pump, Box::new(child))
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
        master.resize(size)
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

impl PtyHandoff for PortablePty {
    type Token = PtyToken;

    fn export(&mut self) -> Result<Vec<(SessionId, PtyToken)>, PtyError> {
        let paused = self.pause_all();
        let mut tokens = Vec::new();
        let mut failure = None;
        for (session, pid) in &paused {
            let carried = pid
                .ok_or_else(|| std::io::Error::other("the child has no pid"))
                .and_then(|pid| Ok((pid, self.carried_fds(session)?)));
            match carried {
                Ok((pid, (master, slave))) => {
                    let token = PtyToken {
                        pid,
                        master: master.into_raw_fd(),
                        slave: slave.into_raw_fd(),
                    };
                    tokens.push((session.clone(), token));
                }
                Err(e) => {
                    failure = Some(io_error(format!("exporting {session}: {e}")));
                    break;
                }
            }
        }
        if let Some(error) = failure {
            for (_, token) in tokens {
                close_fds(&token.fds());
            }
            let sessions = lock(&self.sessions);
            for (session, _) in &paused {
                if let Some(entry) = sessions.get(session) {
                    let _ = entry.live.control.send(Control::Resume);
                }
            }
            return Err(error);
        }
        let mut sessions = lock(&self.sessions);
        for (session, token) in &tokens {
            if let Some(entry) = sessions.remove(session) {
                let carried = token.fds();
                self.exported
                    .insert(session.clone(), Exported { entry, carried });
            }
        }
        tokens.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(tokens)
    }

    fn resume(&mut self) {
        let mut sessions = lock(&self.sessions);
        for (session, exported) in self.exported.drain() {
            close_fds(&exported.carried);
            let _ = exported.entry.live.control.send(Control::Resume);
            sessions.insert(session, exported.entry);
        }
    }

    fn adopt(&mut self, session: SessionId, token: PtyToken) -> Result<(), PtyError> {
        let sessions_handle = self.sessions.clone();
        let mut sessions = lock(&sessions_handle);
        if sessions.contains_key(&session) || self.exported.contains_key(&session) {
            refuse(token);
            return Err(PtyError::AlreadyExists);
        }
        let pid = match libc::pid_t::try_from(token.pid) {
            Ok(pid) if pid > 0 => pid,
            _ => {
                close_fds(&token.fds());
                return Err(io_error(format!("pid {} is not a process", token.pid)));
            }
        };
        let checked = check_adoptable(pid, &token);
        if let Err(e) = checked {
            refuse(token);
            return Err(io_error(e));
        }
        // SAFETY: checked open above; the token hands us their ownership.
        let (master, slave) = unsafe {
            (
                OwnedFd::from_raw_fd(token.master),
                OwnedFd::from_raw_fd(token.slave),
            )
        };
        let handles = (|| {
            let reader = File::from(master.try_clone()?);
            let writer = File::from(master.try_clone()?);
            std::io::Result::Ok((reader, writer))
        })();
        let (reader, writer) = match handles {
            Ok(handles) => handles,
            Err(e) => {
                drop((master, slave));
                kill_and_reap(pid);
                return Err(io_error(e));
            }
        };
        let fd = master.as_raw_fd();
        let (control, controls) = mpsc::channel();
        let live = Arc::new(Live {
            master: Mutex::new(Box::new(RawMaster(master))),
            writer: Mutex::new(Box::new(writer)),
            control,
            slave: SlaveSource::Fd(slave),
        });
        let pump = Pump {
            session: session.clone(),
            live,
            fd,
            reader: Box::new(reader),
            slave: Box::new(()),
            controls,
            unacked: 0,
            sessions: self.sessions.clone(),
            sink: self.sink.clone(),
        };
        self.start_pump(&mut sessions, session, pump, Box::new(RawChild(pid)))
    }
}

impl Drop for PortablePty {
    /// Kills every live session and waits for its pump to finish, so no process or thread outlives the port.
    /// Each session still sends its `Exited`. Exported sessions are someone else's: their pumps are released and
    /// their sessions left running.
    fn drop(&mut self) {
        let entries: Vec<Entry> = lock(&self.sessions).drain().map(|(_, e)| e).collect();
        for entry in &entries {
            let _ = entry.live.control.send(Control::Kill);
        }
        let exported: Vec<Exported> = self.exported.drain().map(|(_, e)| e).collect();
        for exported in &exported {
            let _ = exported.entry.live.control.send(Control::Release);
        }
        for entry in entries
            .into_iter()
            .chain(exported.into_iter().map(|e| e.entry))
        {
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
    /// Held until the drain is done (see the module docs).
    slave: Box<dyn Send>,
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

/// What a control message asks of the pump's loop.
enum Asked {
    Nothing,
    Kill,
    /// Released while paused: stop now.
    Stop,
}

impl Pump {
    fn run(mut self, mut child: Box<dyn Proc>) {
        let pid = child.pid();
        let mut output_open = true;
        let mut killing = Killing::No;

        let code = loop {
            match child.try_wait() {
                Ok(Some(code)) => break code,
                Ok(None) => {}
                Err(_) => break None,
            }

            let mut asked = Vec::new();
            let killed = !matches!(killing, Killing::No);
            // A dying child may be stuck in `exit` until its output is read (macOS drains the tty on close), so
            // once it is being killed it is read whatever the credit.
            if output_open && (self.unacked < OUTPUT_CREDIT || killed) {
                if readable(self.fd, TICK) {
                    output_open = self.forward(killed);
                }
            } else if let Ok(control) = self.controls.recv_timeout(TICK) {
                asked.push(self.apply(control, pid));
            }
            while let Ok(control) = self.controls.try_recv() {
                asked.push(self.apply(control, pid));
            }
            if asked.iter().any(|a| matches!(a, Asked::Stop)) {
                return self.release();
            }
            let kill_requested = asked.iter().any(|a| matches!(a, Asked::Kill));

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

    /// Takes in one control message. A pause is served here, until a resume or a release.
    fn apply(&mut self, control: Control, pid: Option<u32>) -> Asked {
        match control {
            Control::Kill => Asked::Kill,
            Control::Ack(n) => {
                self.unacked = self.unacked.saturating_sub(n);
                Asked::Nothing
            }
            Control::Pause(answer) => self.paused(answer, pid),
            Control::Resume | Control::Release => Asked::Nothing,
        }
    }

    /// Paused: everything read so far has been sent. Reads, reaps and signals nothing until resumed or released.
    fn paused(&mut self, answer: Sender<Option<u32>>, pid: Option<u32>) -> Asked {
        let _ = answer.send(pid);
        let mut kill = false;
        loop {
            match self.controls.recv() {
                Ok(Control::Resume) => {
                    return if kill { Asked::Kill } else { Asked::Nothing };
                }
                // Nobody can resume it: let it go rather than hang.
                Ok(Control::Release) | Err(_) => return Asked::Stop,
                Ok(Control::Ack(n)) => self.unacked = self.unacked.saturating_sub(n),
                Ok(Control::Kill) => kill = true,
                Ok(Control::Pause(again)) => {
                    let _ = again.send(pid);
                }
            }
        }
    }

    /// The session is someone else's: stop without reaping or signalling it. The writer is leaked rather than
    /// dropped, because dropping portable-pty's writer types an EOF into the session.
    fn release(self) {
        let writer = std::mem::replace(&mut *lock(&self.live.writer), Box::new(std::io::sink()));
        std::mem::forget(writer);
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

/// Whether a token's descriptors are open and its pid is our own child (exited or not, without reaping it).
fn check_adoptable(pid: libc::pid_t, token: &PtyToken) -> Result<(), String> {
    for (what, fd) in [("master", token.master), ("slave", token.slave)] {
        // SAFETY: plain syscall; only reads the descriptor's flags.
        if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return Err(format!("the {what} descriptor {fd} is not open"));
        }
        // SAFETY: plain syscall.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(format!("the {what} descriptor {fd} is not a terminal"));
        }
    }
    // SAFETY: an all-zero siginfo_t is a valid out-parameter.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    let id = libc::id_t::try_from(pid).map_err(|e| e.to_string())?;
    // SAFETY: plain syscall with a valid out-parameter. WNOWAIT leaves an exited child to be reaped later.
    if unsafe { libc::waitid(libc::P_PID, id, &mut info, flags) } == -1 {
        let e = std::io::Error::last_os_error();
        return Err(format!("process {pid} is not ours to adopt: {e}"));
    }
    Ok(())
}

/// A token that won't be adopted: closes its descriptors and kills its process group, so nothing is left running
/// unattended.
fn refuse(token: PtyToken) {
    close_fds(&token.fds());
    if let Ok(pid) = libc::pid_t::try_from(token.pid)
        && pid > 0
    {
        kill_and_reap(pid);
    }
}

/// Kills `pid`'s process group, and reaps `pid` in the background if it is our child: on macOS a dying child can
/// wait in `exit` for its terminal's output to be read.
fn kill_and_reap(pid: libc::pid_t) {
    // SAFETY: plain syscall. The child is a session leader, so its pid is its process group.
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    let _ = thread::Builder::new()
        .name(format!("pty-reap-{pid}"))
        .spawn(move || {
            let mut status = 0;
            // SAFETY: plain syscall; fails harmlessly with ECHILD if it isn't our child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
        });
}

/// Closes a token's descriptors. One that isn't open (a bad token) is skipped: `close` just fails with `EBADF`.
fn close_fds(fds: &[RawFd]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: plain syscall on a descriptor the token gave us to own.
            unsafe { libc::close(fd) };
        }
    }
}

/// A new descriptor for `fd`, close-on-exec.
fn dup_cloexec(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: plain syscall on an open fd.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
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

    use std::time::{Duration, Instant};

    use shepherd_core::ids::SessionId;
    use shepherd_core::ports::{Pty, PtyError, PtyEvent, PtyHandoff};
    use shepherd_testkit::contracts::pty::{ChannelSink, spec};

    use super::{PortablePty, close_fds};

    /// Whether `pid` is gone (reaped), within a generous liveness bound.
    fn gone(pid: u32) -> bool {
        let pid = libc::pid_t::try_from(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            // SAFETY: plain syscall; signal 0 only checks the pid exists.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    #[test]
    fn a_token_whose_terminal_is_gone_is_refused_and_its_process_killed() {
        let (tx, rx) = mpsc::channel();
        let mut old = PortablePty::new(Arc::new(ChannelSink(tx.clone())));
        let s = SessionId::new("orphan");
        old.spawn(s.clone(), spec("sh", &["-c", "echo ready; cat"]))
            .unwrap();
        while !matches!(rx.recv().unwrap(), PtyEvent::Output { .. }) {}
        let (_, token) = old.export().unwrap().remove(0);
        close_fds(&[token.master]);

        let mut new = PortablePty::new(Arc::new(ChannelSink(tx)));
        assert!(matches!(new.adopt(s.clone(), token), Err(PtyError::Io(_))));
        assert_eq!(new.write(&s, b"x"), Err(PtyError::UnknownSession));
        assert!(gone(token.pid), "process {} still running", token.pid);
    }

    shepherd_testkit::pty_contract!(PortablePty::new);

    mod handoff {
        use super::PortablePty;

        shepherd_testkit::pty_handoff_contract!(PortablePty::new);
    }

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
