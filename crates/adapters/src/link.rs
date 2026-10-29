//! `Link` over a unix socket. The host binds with [`UnixListenLink::bind`] (normally `~/.shepherd/host.sock`), and
//! clients connect with [`UnixConnectLink::connect`].
//!
//! Each conn has a reader thread and a writer thread. The writer drains an unbounded channel, while `send`
//! counts the bytes waiting in it and disconnects the conn as a slow consumer once that passes `MAX_QUEUED`, so
//! `send` never blocks. The reader splits the stream with `wire::Decoder` and is the only thread that emits the
//! conn's `Line`s and its `Disconnected`, so `Disconnected` is sent once, after every `Line`. Whoever decides a
//! conn is over (close, slow consumer, bad line, write error) records the reason, first one wins, and shuts the
//! socket down, which ends the reader.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    ConnId, DisconnectReason, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED,
};
use shepherd_core::wire::Decoder;

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
pub struct UnixListenLink {
    conns: Conns,
    path: PathBuf,
    stopping: Arc<AtomicBool>,
}

impl UnixListenLink {
    /// Listens on `path`. A socket file nobody is listening on (left by a host that died) is replaced. A live one
    /// is [`UnixLinkError::AddrInUse`] and is left alone.
    pub fn bind(
        path: impl AsRef<Path>,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<Self, UnixLinkError> {
        let path = path.as_ref().to_path_buf();
        check_len(&path)?;
        let listener = match UnixListener::bind(&path) {
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                replace_stale(&path)?;
                UnixListener::bind(&path)?
            }
            other => other?,
        };

        let conns = Conns::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (accept_conns, accept_stopping) = (conns.clone(), stopping.clone());
        thread::Builder::new()
            .name("link-accept".into())
            .spawn(move || accept(listener, accept_conns, sink, accept_stopping))?;
        Ok(Self {
            conns,
            path,
            stopping,
        })
    }
}

impl Link for UnixListenLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        self.conns.send(conn, line)
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.conns.close(conn)
    }
}

impl Drop for UnixListenLink {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wakes the accept thread so it sees `stopping`.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
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

fn accept(
    listener: UnixListener,
    conns: Conns,
    sink: Arc<dyn LinkEventSink>,
    stopping: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        // A failed accept (e.g. out of fds) loses that client only.
        let Ok(stream) = stream else { continue };
        let _ = conns.open(stream, sink.clone(), true);
        // The link was dropped while this conn was being set up, after it disconnected the others.
        if stopping.load(Ordering::SeqCst) {
            conns.drop_all();
            return;
        }
    }
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
    /// When `close` was called. From then on no `Line`s are emitted.
    closed_at: OnceLock<Instant>,
}

impl Shared {
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
            closed_at: OnceLock::new(),
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
        let spawned = thread::Builder::new()
            .name(format!("link-write-{}", conn.get()))
            .spawn(move || write_loop(writer_stream, rx, writer_shared))
            .and_then(|_| {
                let conns = self.clone();
                thread::Builder::new()
                    .name(format!("link-read-{}", conn.get()))
                    .spawn(move || read_loop(stream, conn, shared, conns, sink))
            });
        if let Err(e) = spawned {
            if let Some(handle) = self.lock().remove(&conn) {
                handle.shared.end(DisconnectReason::PeerGone);
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

    fn close(&self, conn: ConnId) -> Result<(), LinkError> {
        let handle = self.lock().remove(&conn).ok_or(LinkError::UnknownConn)?;
        let _ = handle.shared.reason.set(DisconnectReason::Closed);
        let _ = handle.shared.closed_at.set(Instant::now());
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
                Ok(line) if shared.closed_at.get().is_none() => {
                    sink.send(LinkEvent::Line { conn, line });
                }
                Ok(_) => {}
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
                    .closed_at
                    .get()
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

    /// A tempdir under `/tmp`, so socket paths stay well under the `sun_path` limit (macOS's `$TMPDIR` is long).
    fn short_tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("shl")
            .tempdir_in("/tmp")
            .unwrap()
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

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = short_tempdir();
        let path = dir.path().join("host.sock");
        // A listener that goes away without removing its file, like a host that crashed.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());

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
