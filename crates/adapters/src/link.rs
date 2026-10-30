//! `Link` over a unix socket. The host binds with [`UnixListenLink::bind`] (normally `~/.shepherd/host.sock`), and
//! clients connect with [`UnixConnectLink::connect`].
//!
//! Each conn has a reader thread and a writer thread. The writer drains an unbounded channel, while `send`
//! counts the bytes waiting in it and disconnects the conn as a slow consumer once that passes `MAX_QUEUED`, so
//! `send` never blocks. The reader splits the stream with `wire::Decoder` and is the only thread that emits the
//! conn's `Line`s and its `Disconnected`, so `Disconnected` is sent once, after every `Line`. Whoever decides a
//! conn is over (close, slow consumer, bad line, write error) records the reason, first one wins, and shuts the
//! socket down, which ends the reader.
//!
//! Anyone who can connect to the socket can drive the user's agents, so it is private: its directory must not be
//! accessible to group or others (it is created exactly `0700` if missing, and an existing one that is open to
//! others is refused with [`UnixLinkError::InsecureDir`], never chmodded; a symlink there is refused too), and
//! the socket itself is set to `0600` right after it is bound. See [`crate::private_fs`].

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    ConnId, DisconnectReason, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED,
};
use shepherd_core::wire::Decoder;

use crate::private_fs::{self, PrivateFsError};

/// The longest socket path the OS accepts, in bytes (`sizeof(sun_path)` minus the trailing NUL).
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const MAX_SOCKET_PATH: usize = 103;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
pub const MAX_SOCKET_PATH: usize = 107;

/// How long a blocked write waits before checking whether its conn was closed.
const WRITE_TICK: Duration = Duration::from_millis(50);
/// How long `close` keeps trying to deliver queued lines to a peer that isn't reading.
const CLOSE_FLUSH: Duration = Duration::from_secs(2);
/// The writer batches queued lines into writes of about this size.
const WRITE_BATCH: usize = 64 * 1024;
const READ_BUF: usize = 64 * 1024;
/// How long the accept thread waits before retrying after `accept` fails (e.g. out of fds), so it doesn't spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Binding or connecting failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnixLinkError {
    /// A live host is already listening on the path.
    #[error("a host is already listening on {0}")]
    AddrInUse(PathBuf),
    #[error("socket path is {len} bytes, the limit is {max}: {path}")]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    /// The socket's directory exists and group or others have some access to it, so they could reach the socket.
    /// The directory is left as it is: fix its permissions (`chmod 700`) or pick another path.
    #[error(
        "{dir} has mode {mode:o}; the socket directory must not be accessible to group or others"
    )]
    InsecureDir { dir: PathBuf, mode: u32 },
    /// The path exists and is not a socket. It is never removed.
    #[error("{0} exists and is not a socket")]
    NotASocket(PathBuf),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for UnixLinkError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// The host end: accepts clients and emits `Connected` for each.
///
/// For the host's exec (decision 0018) the listener can be [handed over](UnixListenLink::hand_over): the link stops
/// accepting and gives up its descriptor, so connects wait in the listen backlog, which survives the exec, until the
/// new image [adopts](UnixListenLink::adopt) it. A handed-over link never closes the listener or removes the socket
/// file, unless it [takes it back](UnixListenLink::take_back).
pub struct UnixListenLink {
    conns: Conns,
    path: PathBuf,
    /// The socket file's `(dev, ino)`, so `Drop` removes only the file this link created.
    file_id: (u64, u64),
    sink: Arc<dyn LinkEventSink>,
    listener: Listening,
}

enum Listening {
    Accepting(Acceptor),
    /// Handed over: the descriptor is someone else's until taken back.
    HandedOver(RawFd),
}

/// The accept thread, which gives the listener back when it stops.
struct Acceptor {
    /// Shut down to wake the accept thread.
    wake: UnixStream,
    thread: JoinHandle<UnixListener>,
}

impl Acceptor {
    fn start(
        listener: UnixListener,
        conns: Conns,
        sink: Arc<dyn LinkEventSink>,
    ) -> std::io::Result<Self> {
        listener.set_nonblocking(true)?;
        let (wake, wake_rx) = UnixStream::pair()?;
        let thread = thread::Builder::new()
            .name("link-accept".into())
            .spawn(move || accept(listener, wake_rx, conns, sink))?;
        Ok(Self { wake, thread })
    }

    /// Stops accepting. `None` if the thread panicked, taking the listener with it.
    fn stop(self) -> Option<UnixListener> {
        let _ = self.wake.shutdown(Shutdown::Both);
        self.thread.join().ok()
    }
}

impl UnixListenLink {
    /// Listens on `path`, creating its directory (`0700`) if missing. A socket file nobody is listening on (left by
    /// a host that died) is replaced. A live one is [`UnixLinkError::AddrInUse`] and is left alone. An existing
    /// directory that group or others can access is [`UnixLinkError::InsecureDir`].
    pub fn bind(
        path: impl AsRef<Path>,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<Self, UnixLinkError> {
        let path = path.as_ref().to_path_buf();
        check_len(&path)?;
        ensure_private_dir(&path)?;
        let listener = match UnixListener::bind(&path) {
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                replace_stale(&path)?;
                UnixListener::bind(&path)?
            }
            other => other?,
        };
        let file_id = match make_private(&path) {
            Ok(id) => id,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
        };
        Self::serve(path, file_id, listener, sink)
    }

    /// Accepts on `listener`, a descriptor of the socket listening at `path` that a previous image of this process
    /// handed over and carried across its exec. Clients that connected meanwhile are accepted first. A descriptor of
    /// anything else is refused with [`UnixLinkError::Io`].
    pub fn adopt(
        path: impl AsRef<Path>,
        listener: OwnedFd,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<Self, UnixLinkError> {
        let path = path.as_ref().to_path_buf();
        let listener = UnixListener::from(listener);
        let bound = listener.local_addr()?;
        if bound.as_pathname() != Some(path.as_path()) {
            return Err(UnixLinkError::Io(format!(
                "the carried listener is not bound to {}",
                path.display()
            )));
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.file_type().is_socket() {
            return Err(UnixLinkError::NotASocket(path));
        }
        Self::serve(path, (meta.dev(), meta.ino()), listener, sink)
    }

    fn serve(
        path: PathBuf,
        file_id: (u64, u64),
        listener: UnixListener,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<Self, UnixLinkError> {
        let conns = Conns::default();
        let acceptor = Acceptor::start(listener, conns.clone(), sink.clone())?;
        Ok(Self {
            conns,
            path,
            file_id,
            sink,
            listener: Listening::Accepting(acceptor),
        })
    }

    /// Stops accepting and gives up the listener's descriptor (still `CLOEXEC`), for the host's exec to carry. Conns
    /// already accepted are untouched. Connects from now on wait in the backlog.
    pub fn hand_over(&mut self) -> Result<RawFd, UnixLinkError> {
        let fd = match std::mem::replace(&mut self.listener, Listening::HandedOver(-1)) {
            Listening::HandedOver(fd) => fd,
            Listening::Accepting(acceptor) => acceptor
                .stop()
                .ok_or_else(|| UnixLinkError::Io("the accept thread panicked".into()))?
                .into_raw_fd(),
        };
        self.listener = Listening::HandedOver(fd);
        Ok(fd)
    }

    /// Undoes [`hand_over`](Self::hand_over): accepts again, the backlog first.
    pub fn take_back(&mut self) -> Result<(), UnixLinkError> {
        if let Listening::HandedOver(fd) = self.listener {
            // SAFETY: handed over by this link and, the handover called off, ours again.
            let listener = UnixListener::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let acceptor = Acceptor::start(listener, self.conns.clone(), self.sink.clone())?;
            self.listener = Listening::Accepting(acceptor);
        }
        Ok(())
    }
}

/// Makes sure the socket's directory is private, creating it exactly `0700` if it doesn't exist.
fn ensure_private_dir(path: &Path) -> Result<(), UnixLinkError> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    private_fs::ensure_private_dir(dir).map_err(|e| match e {
        PrivateFsError::Insecure { path, mode } => UnixLinkError::InsecureDir { dir: path, mode },
        other => UnixLinkError::Io(other.to_string()),
    })
}

/// Sets the freshly bound socket to `0600`. Returns the socket file's identity.
fn make_private(path: &Path) -> std::io::Result<(u64, u64)> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let meta = std::fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

impl Link for UnixListenLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        self.conns.send(conn, line)
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.conns.close(conn)
    }
    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.conns.queued_bytes(conn)
    }
}

impl Drop for UnixListenLink {
    fn drop(&mut self) {
        let handed_over = match std::mem::replace(&mut self.listener, Listening::HandedOver(-1)) {
            // Once stopped and dropped, the listener is closed and no conn can be added behind `drop_all`.
            Listening::Accepting(acceptor) => {
                drop(acceptor.stop());
                false
            }
            Listening::HandedOver(_) => true,
        };
        let ours = !handed_over
            && std::fs::symlink_metadata(&self.path)
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.file_id);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
        self.conns.drop_all();
    }
}

/// The client end: one conn to a host.
pub struct UnixConnectLink {
    conns: Conns,
}

impl UnixConnectLink {
    pub fn connect(
        path: impl AsRef<Path>,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<(Self, ConnId), UnixLinkError> {
        let path = path.as_ref();
        check_len(path)?;
        let stream = UnixStream::connect(path)?;
        let conns = Conns::default();
        let conn = conns.open(stream, sink, false)?;
        Ok((Self { conns }, conn))
    }
}

impl Link for UnixConnectLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        self.conns.send(conn, line)
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.conns.close(conn)
    }
    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.conns.queued_bytes(conn)
    }
}

impl Drop for UnixConnectLink {
    fn drop(&mut self) {
        self.conns.drop_all();
    }
}

fn check_len(path: &Path) -> Result<(), UnixLinkError> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(UnixLinkError::PathTooLong {
            path: path.to_path_buf(),
            len,
            max: MAX_SOCKET_PATH,
        });
    }
    Ok(())
}

/// Removes the socket at `path` if nothing is listening on it.
fn replace_stale(path: &Path) -> Result<(), UnixLinkError> {
    if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
        return Err(UnixLinkError::NotASocket(path.to_path_buf()));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(UnixLinkError::AddrInUse(path.to_path_buf())),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Accepts clients until `wake` becomes readable (the link stops accepting), then gives the listener back.
/// `listener` is non-blocking.
fn accept(
    listener: UnixListener,
    wake: UnixStream,
    conns: Conns,
    sink: Arc<dyn LinkEventSink>,
) -> UnixListener {
    let mut backoff = false;
    loop {
        let woken = if backoff {
            // After a failed accept, wait for the link to drop or the backoff to pass, not for the listener:
            // it would still be readable and the loop would spin.
            poll_readable(&[wake.as_raw_fd()], Some(ACCEPT_BACKOFF))[0]
        } else {
            poll_readable(&[wake.as_raw_fd(), listener.as_raw_fd()], None)[0]
        };
        if woken {
            return listener;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                backoff = false;
                // Accepted sockets may inherit the listener's non-blocking mode.
                if stream.set_nonblocking(false).is_ok() {
                    let _ = conns.open(stream, sink.clone(), true);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::Interrupted | ErrorKind::ConnectionAborted
                ) => {}
            // E.g. out of fds. Only that client is lost; try again after the backoff.
            Err(_) => backoff = true,
        }
    }
}

/// Waits until one of `fds` is readable (or hung up), or `timeout` passes (`None`: forever). Returns which are.
fn poll_readable(fds: &[RawFd], timeout: Option<Duration>) -> Vec<bool> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = timeout.map_or(-1, |t| {
        libc::c_int::try_from(t.as_millis()).unwrap_or(libc::c_int::MAX)
    });
    let nfds = libc::nfds_t::try_from(pfds.len()).unwrap_or(libc::nfds_t::MAX);
    // SAFETY: `pfds` is a valid, exclusively borrowed array of `nfds` pollfds for the duration of the call.
    let ready = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, ms) };
    if ready <= 0 {
        // Timed out or interrupted: nothing is ready.
        return vec![false; fds.len()];
    }
    pfds.iter().map(|p| p.revents != 0).collect()
}

/// Unique across every link in the process, so unique per link too.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// State shared by a conn's caller-facing handle and its two threads.
struct Shared {
    /// A clone of the conn's socket, for shutting it down.
    stream: UnixStream,
    /// Bytes sent but not yet taken by the writer.
    queued: AtomicUsize,
    /// Why the conn ended. Set once, by whoever ended it first.
    reason: OnceLock<DisconnectReason>,
    /// When `close` was called. The reader holds this lock while it emits a `Line`, so once `close` has set it
    /// and returned, no more `Line`s are emitted.
    closed_at: Mutex<Option<Instant>>,
}

impl Shared {
    fn closed_at(&self) -> MutexGuard<'_, Option<Instant>> {
        self.closed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn end(&self, reason: DisconnectReason) {
        let _ = self.reason.set(reason);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct Handle {
    shared: Arc<Shared>,
    /// Dropping it lets the writer finish what is queued and exit.
    queue: Sender<Vec<u8>>,
}

/// The live conns of one link. A conn is removed when it is closed, dropped as a slow consumer, or right before
/// its `Disconnected` is sent.
#[derive(Clone, Default)]
struct Conns(Arc<Mutex<HashMap<ConnId, Handle>>>);

impl Conns {
    fn lock(&self) -> MutexGuard<'_, HashMap<ConnId, Handle>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `stream` as a new conn and starts its threads. With `announce` (host side), the sink gets
    /// `Connected` first; a client learns its conn from `connect` instead.
    fn open(
        &self,
        stream: UnixStream,
        sink: Arc<dyn LinkEventSink>,
        announce: bool,
    ) -> std::io::Result<ConnId> {
        let conn = ConnId::new(NEXT_CONN.fetch_add(1, Ordering::Relaxed));
        let shared = Arc::new(Shared {
            stream: stream.try_clone()?,
            queued: AtomicUsize::new(0),
            reason: OnceLock::new(),
            closed_at: Mutex::new(None),
        });
        let writer_stream = stream.try_clone()?;
        // Fails (EINVAL on macOS) only when the peer has already gone. The conn is still registered and announced,
        // and its reader reports the disconnect right away.
        let _ = writer_stream.set_write_timeout(Some(WRITE_TICK));
        let (queue, rx) = mpsc::channel();

        self.lock().insert(
            conn,
            Handle {
                shared: shared.clone(),
                queue,
            },
        );
        // Sent before the reader starts, so it comes before the conn's first `Line`.
        if announce {
            sink.send(LinkEvent::Connected { conn });
        }

        let writer_shared = shared.clone();
        let (reader_conns, reader_sink) = (self.clone(), sink.clone());
        let spawned = thread::Builder::new()
            .name(format!("link-write-{}", conn.get()))
            .spawn(move || write_loop(writer_stream, rx, writer_shared))
            .and_then(|_| {
                thread::Builder::new()
                    .name(format!("link-read-{}", conn.get()))
                    .spawn(move || read_loop(stream, conn, shared, reader_conns, reader_sink))
            });
        if let Err(e) = spawned {
            // No reader to send the `Disconnected` that must follow `Connected`, so send it here. A writer that
            // did start exits once the handle (and with it the queue) is dropped.
            if let Some(handle) = self.lock().remove(&conn) {
                handle.shared.end(DisconnectReason::PeerGone);
            }
            if announce {
                sink.send(LinkEvent::Disconnected {
                    conn,
                    reason: DisconnectReason::PeerGone,
                });
            }
            return Err(e);
        }
        Ok(conn)
    }

    fn send(&self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        let mut conns = self.lock();
        let handle = conns.get(&conn).ok_or(LinkError::UnknownConn)?;
        if line.contains(&b'\n') {
            return Err(LinkError::InvalidLine);
        }
        let queued = handle.shared.queued.load(Ordering::SeqCst);
        if queued > 0 && queued + line.len() > MAX_QUEUED {
            if let Some(handle) = conns.remove(&conn) {
                handle.shared.end(DisconnectReason::SlowConsumer);
            }
            return Ok(());
        }
        let mut framed = Vec::with_capacity(line.len() + 1);
        framed.extend_from_slice(line);
        framed.push(b'\n');
        handle
            .shared
            .queued
            .fetch_add(framed.len(), Ordering::SeqCst);
        // Fails only once the writer is gone, and then the reader is about to report the disconnect.
        let _ = handle.queue.send(framed);
        Ok(())
    }

    /// Bytes sent to `conn` that its writer hasn't taken yet. The batch the writer is writing and what the kernel
    /// buffers are not counted.
    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        let conns = self.lock();
        let handle = conns.get(&conn)?;
        Some(handle.shared.queued.load(Ordering::SeqCst))
    }

    fn close(&self, conn: ConnId) -> Result<(), LinkError> {
        let handle = self.lock().remove(&conn).ok_or(LinkError::UnknownConn)?;
        let _ = handle.shared.reason.set(DisconnectReason::Closed);
        // Waits for a `Line` being emitted right now; none follow.
        *handle.shared.closed_at() = Some(Instant::now());
        // Dropping the handle drops the queue's sender: the writer flushes, then shuts the socket down.
        Ok(())
    }

    fn drop_all(&self) {
        for (_, handle) in self.lock().drain() {
            handle.shared.end(DisconnectReason::Closed);
        }
    }
}

fn read_loop(
    mut stream: UnixStream,
    conn: ConnId,
    shared: Arc<Shared>,
    conns: Conns,
    sink: Arc<dyn LinkEventSink>,
) {
    let mut decoder = Decoder::new();
    let mut buf = vec![0; READ_BUF];
    'read: loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        decoder.push(&buf[..n]);
        while let Some(line) = decoder.next_line() {
            match line {
                Ok(line) => {
                    let closed = shared.closed_at();
                    if closed.is_none() {
                        sink.send(LinkEvent::Line { conn, line });
                    }
                }
                Err(too_long) => {
                    shared.end(DisconnectReason::Protocol(too_long.to_string()));
                    break 'read;
                }
            }
        }
    }
    conns.lock().remove(&conn);
    let reason = shared
        .reason
        .get()
        .cloned()
        .unwrap_or(DisconnectReason::PeerGone);
    sink.send(LinkEvent::Disconnected { conn, reason });
}

fn write_loop(mut stream: UnixStream, queue: Receiver<Vec<u8>>, shared: Arc<Shared>) {
    let mut batch = Vec::with_capacity(WRITE_BATCH);
    while let Ok(first) = queue.recv() {
        take(&shared, &first);
        batch.clear();
        batch.extend_from_slice(&first);
        while batch.len() < WRITE_BATCH {
            match queue.try_recv() {
                Ok(next) => {
                    take(&shared, &next);
                    batch.extend_from_slice(&next);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if write_all(&mut stream, &batch, &shared).is_err() {
            shared.end(DisconnectReason::PeerGone);
            return;
        }
    }
    // The handle was dropped: closed, slow consumer, or the link went away.
    let _ = stream.shutdown(Shutdown::Both);
}

fn take(shared: &Shared, line: &[u8]) {
    shared.queued.fetch_sub(line.len(), Ordering::SeqCst);
}

/// Like `Write::write_all`, but gives up once a close has waited `CLOSE_FLUSH` for a peer that isn't reading.
fn write_all(stream: &mut UnixStream, mut bytes: &[u8], shared: &Shared) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if shared
                    .closed_at()
                    .is_some_and(|at| at.elapsed() > CLOSE_FLUSH)
                {
                    return Err(e);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepherd_testkit::contracts::link::{Events, LinkFixture};
    use tempfile::TempDir;

    /// A private (`0700`) tempdir under `/tmp`, so socket paths stay well under the `sun_path` limit (macOS's
    /// `$TMPDIR` is long).
    fn short_tempdir() -> TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("shl")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    struct UnixFixture {
        dir: TempDir,
    }

    impl UnixFixture {
        fn new() -> Self {
            Self {
                dir: short_tempdir(),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("host.sock")
        }
    }

    impl LinkFixture for UnixFixture {
        type Host = UnixListenLink;
        type Client = UnixConnectLink;
        /// A raw socket: never read, and dropping it closes the fd with no shutdown.
        type Stalled = UnixStream;

        fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> UnixListenLink {
            UnixListenLink::bind(self.path(), sink).unwrap()
        }

        fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (UnixConnectLink, ConnId) {
            UnixConnectLink::connect(self.path(), sink).unwrap()
        }

        fn stalled_peer(&mut self) -> UnixStream {
            UnixStream::connect(self.path()).unwrap()
        }
    }

    mod contract {
        use super::UnixFixture;

        shepherd_testkit::link_contract!(UnixFixture::new());
    }

    fn bind(path: &Path) -> Result<(UnixListenLink, Events), UnixLinkError> {
        let (sink, events) = Events::channel();
        UnixListenLink::bind(path, sink).map(|link| (link, events))
    }

    /// Waits until nothing holds the listener at `path` open. macOS has no `SOCK_CLOEXEC`, so a PTY forked by a
    /// test running in parallel can briefly hold a copy of a dropped listener's fd, and connects still succeed.
    fn wait_until_refused(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match UnixStream::connect(path) {
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => return,
                _ if std::time::Instant::now() > deadline => {
                    panic!("{} still accepts connections", path.display())
                }
                _ => {}
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Exact modes, so a missing chmod or `DirBuilder::mode` shows up under the ordinary umask too (the socket
    /// would be 0755 or so, the dir 0755).
    #[test]
    fn socket_is_0600_and_created_dir_is_0700() {
        let dir = short_tempdir();
        let parent = dir.path().join("shepherd");
        let path = parent.join("host.sock");

        let (_host, _events) = bind(&path).unwrap();

        assert_eq!(mode(&parent), 0o700, "dir mode {:o}", mode(&parent));
        assert_eq!(mode(&path), 0o600, "socket mode {:o}", mode(&path));
    }

    /// Reruns the test above in a child under a permissive umask (000) and one that strips owner bits (277),
    /// since the process umask is never changed: the created dir must be exactly 0700 either way.
    #[test]
    fn created_dir_is_0700_whatever_the_umask() {
        let exe = std::env::current_exe().unwrap();
        for umask in ["000", "277"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "umask {umask} && exec \"$0\" --exact --test-threads=1 \"$1\""
                ))
                .arg(&exe)
                .arg("link::tests::socket_is_0600_and_created_dir_is_0700")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "umask {umask}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A symlink planted where the socket's dir goes is refused even when it points at a private dir: it is
    /// never followed, and its target is never chmodded or bound into.
    #[test]
    fn socket_dir_that_is_a_symlink_is_refused_and_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        for target_mode in [0o700, 0o755] {
            let dir = short_tempdir();
            let target = dir.path().join("elsewhere");
            std::fs::create_dir(&target).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(target_mode))
                .unwrap();
            let parent = dir.path().join("shepherd");
            std::os::unix::fs::symlink(&target, &parent).unwrap();

            let result = bind(&parent.join("host.sock")).map(|_| ());
            assert!(
                matches!(&result, Err(UnixLinkError::Io(msg)) if msg.contains("symlink")),
                "{result:?}"
            );
            assert_eq!(mode(&target), target_mode);
            assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        }
    }

    #[test]
    fn existing_dir_open_to_others_is_insecure_dir_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = short_tempdir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("host.sock");

        assert_eq!(
            bind(&path).err(),
            Some(UnixLinkError::InsecureDir {
                dir: dir.path().to_path_buf(),
                mode: 0o755
            })
        );
        assert_eq!(mode(dir.path()), 0o755, "the user's dir is not chmodded");
        assert!(!path.exists());
    }

    #[test]
    fn dropping_the_host_stops_listening_even_if_its_socket_file_was_deleted() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        let alias = dir.path().join("alias.sock");
        let (host, _events) = bind(&path).unwrap();
        // The alias reaches the same listener after the original name is gone.
        std::fs::hard_link(&path, &alias).unwrap();
        std::fs::remove_file(&path).unwrap();

        drop(host);
        // Not a single connect: a PTY forked by a parallel test may hold a copy of the fd for a moment.
        wait_until_refused(&alias);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        // A listener that goes away without removing its file, like a host that crashed.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        wait_until_refused(&path);

        let (_host, events) = bind(&path).unwrap();
        let (sink, _client_events) = Events::channel();
        let _client = UnixConnectLink::connect(&path, sink).unwrap();
        events.wait_for(
            |e| matches!(e, LinkEvent::Connected { .. }),
            Duration::from_secs(10),
        );
    }

    #[test]
    fn live_socket_is_addr_in_use_and_left_alone() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        let (_first, first_events) = bind(&path).unwrap();

        assert_eq!(
            bind(&path).err(),
            Some(UnixLinkError::AddrInUse(path.clone()))
        );
        // The first host still gets its clients.
        let (sink, _client_events) = Events::channel();
        let _client = UnixConnectLink::connect(&path, sink).unwrap();
        first_events.wait_for(
            |e| matches!(e, LinkEvent::Connected { .. }),
            Duration::from_secs(10),
        );
    }

    #[test]
    fn existing_file_that_is_not_a_socket_is_never_removed() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        std::fs::write(&path, "precious").unwrap();

        assert_eq!(
            bind(&path).err(),
            Some(UnixLinkError::NotASocket(path.clone()))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    }

    #[test]
    fn socket_path_over_the_os_limit_is_path_too_long() {
        let dir = short_tempdir();
        let path = dir.path().join("x".repeat(MAX_SOCKET_PATH));
        assert!(matches!(
            bind(&path).err(),
            Some(UnixLinkError::PathTooLong {
                max: MAX_SOCKET_PATH,
                ..
            })
        ));
        let (sink, _events) = Events::channel();
        assert!(matches!(
            UnixConnectLink::connect(&path, sink).err(),
            Some(UnixLinkError::PathTooLong { .. })
        ));
    }

    #[test]
    fn dropping_the_host_removes_its_socket_file() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        let (host, _events) = bind(&path).unwrap();
        drop(host);
        assert!(!path.exists());
    }

    /// Waits for the first line on any conn.
    fn first_line(events: &Events) -> Vec<u8> {
        let seen = events.wait_for(
            |e| matches!(e, LinkEvent::Line { .. }),
            Duration::from_secs(10),
        );
        match seen.last() {
            Some(LinkEvent::Line { line, .. }) => line.clone(),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn a_connect_during_the_hand_over_is_accepted_by_the_adopter() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        let (mut old, _old_events) = bind(&path).unwrap();
        let listener = old.hand_over().unwrap();

        // In the backlog: nobody is accepting.
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"during\n").unwrap();
        // Exec never runs the old link's Drop; a handed-over link that is dropped mustn't unlink the socket either.
        drop(old);
        assert!(path.exists());

        let (sink, events) = Events::channel();
        // SAFETY: handed over to us, and nothing else owns it now.
        let listener = unsafe { OwnedFd::from_raw_fd(listener) };
        let _new = UnixListenLink::adopt(&path, listener, sink).unwrap();
        // Had the old link accepted it, dropping the old link would have closed it.
        assert_eq!(first_line(&events), b"during");
    }

    #[test]
    fn a_link_that_takes_its_listener_back_accepts_again() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        let (mut host, events) = bind(&path).unwrap();
        host.hand_over().unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"queued\n").unwrap();

        host.take_back().unwrap();
        assert_eq!(first_line(&events), b"queued");
        drop(host);
        assert!(!path.exists());
    }

    #[test]
    fn adopting_a_socket_bound_elsewhere_is_refused() {
        let dir = short_tempdir();
        let other = UnixListener::bind(dir.path().join("other.sock")).unwrap();
        let (sink, _events) = Events::channel();
        let adopted =
            UnixListenLink::adopt(dir.path().join("host.sock"), OwnedFd::from(other), sink);
        assert!(matches!(adopted.err(), Some(UnixLinkError::Io(_))));
    }

    #[test]
    fn connecting_with_no_host_is_an_error() {
        let dir = short_tempdir();
        let (sink, _events) = Events::channel();
        assert!(matches!(
            UnixConnectLink::connect(dir.path().join("host.sock"), sink).err(),
            Some(UnixLinkError::Io(_))
        ));
    }
}
