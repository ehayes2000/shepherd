//! `Link` between machines, on the host's iroh endpoint (T31a, decision 0019). It is built on the [`IrohPeers`] that
//! owns the endpoint ([`IrohPeers::link`]): conns travel over the connections that port holds with trusted peers, so
//! trust is the port's (a machine outside the trusted set never gets this far, and a dial it refuses ends the conn as
//! [`DisconnectReason::Refused`]).
//!
//! - **A conn is one bidirectional QUIC stream** on a connection with its peer, whichever side dialled it. The side
//!   that [opens](Link::open) it (dialling the peer first if no connection is held) writes [`PREAMBLE`]; the other side
//!   reads it, announces the conn (`Connected`) and answers [`ACK`], and the opener announces it once it reads that.
//!   Lines sent to a conn before then wait in its queue. Each end knows the other's machine ([`Link::peer`]): the
//!   connection's authenticated node id.
//! - **Each conn has a reader task and a writer task** on the port's runtime thread, like the unix adapter's threads.
//!   The writer drains an unbounded queue; `send` counts what waits in it and ends the conn as a slow consumer past
//!   [`MAX_QUEUED`]. Every byte it writes is counted as drained. The reader splits the stream with `wire::Decoder` and
//!   is the only task that emits the conn's `Line`s and its one `Disconnected`, after them. While the conn is paused it
//!   reads nothing more (it looks between reads and between lines), so the peer's QUIC flow-control window fills and
//!   the peer's writer backs up into its own queue.
//! - **Closing** drops the queue: the writer writes what is left, finishes the stream and waits until the peer has
//!   it all (at most [`CLOSE_FLUSH`] after the close, for a peer that isn't reading), then stops the reader. A conn
//!   ended for any other reason by this end (slow consumer, a line over the limit, the link dropped) resets its stream
//!   at once. A peer that finishes or resets its stream, or whose connection goes, ends the conn as `PeerGone`.
//! - Each stream's receive window is small ([`STREAM_WINDOW`], see [`IrohPeers`]), so back-pressure reaches the sender
//!   soon and `queued_bytes` means what it says.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    CLOSE_FLUSH, ConnId, DisconnectReason, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink,
    MAX_QUEUED,
};
use shepherd_core::wire::Decoder;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::link::{Closing, Stopped, Written, WrittenOnExit};
#[cfg(doc)]
use crate::peers::IrohPeers;
use crate::peers::PeersHandle;

/// What the opener of a conn writes first.
pub const PREAMBLE: &[u8] = b"shepherd-link/0\n";
/// What the other side answers once it has announced the conn.
pub const ACK: &[u8] = b"ok\n";
/// The receive window of each conn's stream.
pub const STREAM_WINDOW: u32 = 256 * 1024;
/// How long opening a conn may wait for the other side's answer: a bound on a peer that went quiet, not a timing.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(30);
/// How long a blocked write waits before checking whether its conn's close has waited [`CLOSE_FLUSH`].
const WRITE_TICK: Duration = Duration::from_millis(50);
const READ_BUF: usize = 64 * 1024;

/// Unique across every iroh link in the process.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// The state a conn's handle, its reader and its writer share.
pub(crate) struct ConnShared {
    peer: MachineId,
    /// Bytes sent but not yet taken by the writer.
    queued: AtomicUsize,
    /// Bytes the writer has written.
    drained: AtomicU64,
    /// Why the conn ended. Set once, by whoever ended it first.
    reason: OnceLock<DisconnectReason>,
    /// When `close` was called. The reader holds this lock while it emits a `Line`, so once `close` has set it and
    /// returned, no more `Line`s are emitted.
    closed_at: Mutex<Option<Instant>>,
    /// Set when this end ends the conn at once: the reader stops, the writer resets the stream.
    end: watch::Sender<bool>,
    paused: watch::Sender<bool>,
    written: Arc<Written>,
}

impl ConnShared {
    fn new(peer: MachineId) -> Self {
        Self {
            peer,
            queued: AtomicUsize::new(0),
            drained: AtomicU64::new(0),
            reason: OnceLock::new(),
            closed_at: Mutex::new(None),
            end: watch::Sender::new(false),
            paused: watch::Sender::new(false),
            written: Arc::default(),
        }
    }

    fn closed_at(&self) -> MutexGuard<'_, Option<Instant>> {
        self.closed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the conn at once, for `reason` unless it already has one.
    fn end(&self, reason: DisconnectReason) {
        let _ = self.reason.set(reason);
        self.end.send_replace(true);
    }

    /// Whether a close has waited longer than [`CLOSE_FLUSH`] for a peer that isn't reading.
    fn flush_expired(&self) -> bool {
        self.closed_at()
            .is_some_and(|at| at.elapsed() > CLOSE_FLUSH)
    }
}

struct Handle {
    shared: Arc<ConnShared>,
    /// Dropping it lets the writer finish what is queued.
    queue: UnboundedSender<Vec<u8>>,
}

/// A conn being opened: what the port's runtime needs to carry it on once its stream is up.
pub(crate) struct Opening {
    pub(crate) peer: MachineId,
    conn: ConnId,
    shared: Arc<ConnShared>,
    queue: UnboundedReceiver<Vec<u8>>,
    conns: Arc<Conns>,
}

/// The live conns of one link, and the closed ones still delivering.
pub(crate) struct Conns {
    sink: Arc<dyn LinkEventSink>,
    live: Mutex<HashMap<ConnId, Handle>>,
    closing: Closing,
}

impl Conns {
    pub(crate) fn new(sink: Arc<dyn LinkEventSink>) -> Self {
        Self {
            sink,
            live: Mutex::default(),
            closing: Closing::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ConnId, Handle>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a new conn with `peer`. Returns it, its shared state and its queue's receiving end.
    fn add(&self, peer: MachineId) -> (ConnId, Arc<ConnShared>, UnboundedReceiver<Vec<u8>>) {
        let conn = ConnId::new(NEXT_CONN.fetch_add(1, Ordering::Relaxed));
        let shared = Arc::new(ConnShared::new(peer));
        let (queue, rx) = unbounded_channel();
        self.lock().insert(
            conn,
            Handle {
                shared: shared.clone(),
                queue,
            },
        );
        (conn, shared, rx)
    }

    /// A conn this link [opened](Link::open) that never got going ends here, for `reason`.
    fn failed(&self, conn: ConnId, shared: &ConnShared, reason: DisconnectReason) {
        self.lock().remove(&conn);
        let _ = shared.reason.set(reason);
        let reason = shared
            .reason
            .get()
            .cloned()
            .unwrap_or(DisconnectReason::PeerGone);
        // Its writer never ran: whatever was queued for it is as delivered as it will be.
        drop(WrittenOnExit(shared.written.clone()));
        self.sink.send(LinkEvent::Disconnected { conn, reason });
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
        // Waits for a `Line` being emitted right now; none follow.
        *handle.shared.closed_at() = Some(Instant::now());
        // A paused reader reads on, unseen, until the writer is done.
        handle.shared.paused.send_replace(false);
        self.closing.push(handle.shared.written.clone());
        // Dropping the handle drops the queue's sender: the writer flushes, then finishes the stream.
        Ok(())
    }

    fn flush_closed(&self, deadline: Instant) -> FlushOutcome {
        self.closing.flush(deadline)
    }

    fn with<T>(&self, conn: ConnId, f: impl FnOnce(&ConnShared) -> T) -> Option<T> {
        self.lock().get(&conn).map(|handle| f(&handle.shared))
    }

    fn drop_all(&self) {
        for (_, handle) in self.lock().drain() {
            handle.shared.end(DisconnectReason::Closed);
        }
    }

    /// A stream the peer `peer` opened on a connection with this machine: once its preamble is read, it is announced
    /// and answered, and runs as a conn.
    pub(crate) async fn accepted(
        self: Arc<Self>,
        peer: MachineId,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let mut preamble = vec![0; PREAMBLE.len()];
        let read = timeout(HANDSHAKE_LIMIT, recv.read_exact(&mut preamble)).await;
        if !matches!(read, Ok(Ok(()))) || preamble != PREAMBLE {
            let _ = send.reset(VarInt::from_u32(0));
            let _ = recv.stop(VarInt::from_u32(0));
            return;
        }
        let (conn, shared, queue) = self.add(peer);
        // Before anything else of the conn, and before the opener hears it was accepted.
        self.sink.send(LinkEvent::Connected { conn });
        let writer = write_loop(send, queue, shared.clone(), Some(ACK));
        tokio::spawn(writer);
        read_loop(recv, conn, shared, self).await;
    }
}

/// Carries a conn this link opened on over `send`/`recv`, a stream just opened to its peer: writes the preamble,
/// waits for the answer, announces the conn, and runs it. A conn closed meanwhile is never announced, but still
/// delivers what was queued for it and ends with one `Disconnected`.
pub(crate) async fn opened(opening: Opening, mut send: SendStream, mut recv: RecvStream) {
    let Opening {
        conn,
        shared,
        queue,
        conns,
        ..
    } = opening;
    let mut ack = vec![0; ACK.len()];
    let handshake = timeout(HANDSHAKE_LIMIT, async {
        send.write_all(PREAMBLE).await.ok()?;
        recv.read_exact(&mut ack).await.ok()?;
        (ack == ACK).then_some(())
    })
    .await;
    if !matches!(handshake, Ok(Some(()))) {
        let _ = send.reset(VarInt::from_u32(0));
        let _ = recv.stop(VarInt::from_u32(0));
        return conns.failed(conn, &shared, DisconnectReason::PeerGone);
    }
    {
        let closed = shared.closed_at();
        if closed.is_none() && shared.reason.get().is_none() {
            conns.sink.send(LinkEvent::Connected { conn });
        }
    }
    tokio::spawn(write_loop(send, queue, shared.clone(), None));
    read_loop(recv, conn, shared, conns).await;
}

/// Ends `opening` without a stream, for `reason`.
pub(crate) fn not_opened(opening: Opening, reason: DisconnectReason) {
    opening.conns.failed(opening.conn, &opening.shared, reason);
}

/// Waits until `end` is set.
async fn ended(end: &mut watch::Receiver<bool>) {
    let _ = end.wait_for(|end| *end).await;
}

/// Waits until `paused` is cleared.
async fn resumed(paused: &mut watch::Receiver<bool>) {
    let _ = paused.wait_for(|paused| !*paused).await;
}

/// The conn's reader: emits its `Line`s, then its one `Disconnected`.
async fn read_loop(mut recv: RecvStream, conn: ConnId, shared: Arc<ConnShared>, conns: Arc<Conns>) {
    let mut end = shared.end.subscribe();
    let mut paused = shared.paused.subscribe();
    let mut decoder = Decoder::new();
    let mut buf = vec![0; READ_BUF];
    'read: loop {
        tokio::select! {
            biased;
            () = ended(&mut end) => break,
            () = resumed(&mut paused) => {}
        }
        let n = tokio::select! {
            biased;
            () = ended(&mut end) => break,
            read = recv.read(&mut buf) => match read {
                Ok(Some(n)) => n,
                // Finished by the peer, reset, or its connection is gone.
                Ok(None) | Err(_) => break,
            },
        };
        decoder.push(&buf[..n]);
        loop {
            // Paused after this read: what it read waits in the decoder.
            tokio::select! {
                biased;
                () = ended(&mut end) => break 'read,
                () = resumed(&mut paused) => {}
            }
            let Some(line) = decoder.next_line() else {
                break;
            };
            match line {
                Ok(line) => {
                    let closed = shared.closed_at();
                    if closed.is_none() {
                        conns.sink.send(LinkEvent::Line { conn, line });
                    }
                }
                Err(too_long) => {
                    shared.end(DisconnectReason::Protocol(too_long.to_string()));
                    break 'read;
                }
            }
        }
    }
    let _ = recv.stop(VarInt::from_u32(0));
    // Dropping the handle (if it is still live) lets the writer deliver what is queued and finish.
    conns.lock().remove(&conn);
    let reason = shared
        .reason
        .get()
        .cloned()
        .unwrap_or(DisconnectReason::PeerGone);
    conns.sink.send(LinkEvent::Disconnected { conn, reason });
}

/// The conn's writer: `first` (the acceptor's answer), then the queue, until it is dropped; then finishes the stream
/// and waits until the peer has it all, bounded by [`CLOSE_FLUSH`] after a close. Ended at once by `end`.
async fn write_loop(
    mut send: SendStream,
    mut queue: UnboundedReceiver<Vec<u8>>,
    shared: Arc<ConnShared>,
    first: Option<&'static [u8]>,
) {
    let _done = WrittenOnExit(shared.written.clone());
    let mut end = shared.end.subscribe();
    if let Some(first) = first
        && let Err(stopped) = write_all(&mut send, first, &shared, &mut end, false).await
    {
        // Untested: the answer is a few bytes into a fresh stream's window, so it is never held up for `CLOSE_FLUSH`.
        if stopped == Stopped::GaveUp {
            shared.written.give_up();
        }
        return;
    }
    loop {
        let line = tokio::select! {
            biased;
            () = ended(&mut end) => {
                let _ = send.reset(VarInt::from_u32(0));
                return;
            }
            line = queue.recv() => line,
        };
        let Some(line) = line else {
            break;
        };
        shared.queued.fetch_sub(line.len(), Ordering::SeqCst);
        if let Err(stopped) = write_all(&mut send, &line, &shared, &mut end, true).await {
            if stopped == Stopped::GaveUp {
                shared.written.give_up();
            }
            let _ = send.reset(VarInt::from_u32(0));
            return;
        }
    }
    // The queue was dropped: closed, or its reader is done. Deliver, then stop reading.
    if send.finish().is_ok() {
        let left = shared
            .closed_at()
            .map_or(CLOSE_FLUSH, |at| CLOSE_FLUSH.saturating_sub(at.elapsed()));
        tokio::select! {
            biased;
            () = ended(&mut end) => {
                let _ = send.reset(VarInt::from_u32(0));
            }
            stopped = timeout(left, send.stopped()) => {
                if stopped.is_err() {
                    // The peer's transport hadn't acknowledged it all within the bound. Untested: `stopped` resolves
                    // once the peer has the data, read or not (a paused peer included), so only a peer that went
                    // silent without closing gets here, which machines on localhost can't play.
                    shared.written.give_up();
                    let _ = send.reset(VarInt::from_u32(0));
                }
            }
        }
    }
    shared.end.send_replace(true);
}

/// Writes all of `bytes`, counting each write as drained if `count`. Gives up when the conn is ended, the stream
/// fails, or a close has waited [`CLOSE_FLUSH`] for a peer that isn't reading.
async fn write_all(
    send: &mut SendStream,
    mut bytes: &[u8],
    shared: &ConnShared,
    end: &mut watch::Receiver<bool>,
    count: bool,
) -> Result<(), Stopped> {
    while !bytes.is_empty() {
        let wrote = tokio::select! {
            biased;
            () = ended(end) => return Err(Stopped::Failed),
            wrote = timeout(WRITE_TICK, send.write(bytes)) => wrote,
        };
        match wrote {
            Ok(Ok(n)) => {
                if count {
                    shared.drained.fetch_add(n as u64, Ordering::SeqCst);
                }
                bytes = &bytes[n..];
            }
            Ok(Err(_)) => return Err(Stopped::Failed),
            Err(_) if shared.flush_expired() => return Err(Stopped::GaveUp),
            Err(_) => {}
        }
    }
    Ok(())
}

/// `Link` between machines, on an [`IrohPeers`]' endpoint. See the module docs.
pub struct IrohLink {
    conns: Arc<Conns>,
    peers: PeersHandle,
}

impl IrohLink {
    pub(crate) fn new(conns: Arc<Conns>, peers: PeersHandle) -> Self {
        Self { conns, peers }
    }
}

impl Link for IrohLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        self.conns.send(conn, line)
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.conns.close(conn)
    }

    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.conns
            .with(conn, |shared| shared.queued.load(Ordering::SeqCst))
    }

    fn flush_closed(&mut self, within: Duration) -> FlushOutcome {
        self.conns.flush_closed(Instant::now() + within)
    }

    fn peer(&self, conn: ConnId) -> Result<Option<MachineId>, LinkError> {
        self.conns
            .with(conn, |shared| Some(shared.peer.clone()))
            .ok_or(LinkError::UnknownConn)
    }

    fn drained_bytes(&self, conn: ConnId) -> Option<u64> {
        self.conns
            .with(conn, |shared| shared.drained.load(Ordering::SeqCst))
    }

    fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError> {
        self.conns
            .with(conn, |shared| {
                shared.paused.send_replace(paused);
            })
            .ok_or(LinkError::UnknownConn)
    }

    fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError> {
        let (conn, shared, queue) = self.conns.add(peer.clone());
        self.peers.open(Opening {
            peer: peer.clone(),
            conn,
            shared,
            queue,
            conns: self.conns.clone(),
        });
        Ok(conn)
    }
}

impl Drop for IrohLink {
    fn drop(&mut self) {
        self.peers.unlink(&self.conns);
        self.conns.drop_all();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use shepherd_core::ports::{MAX_OPEN_PER_PEER, MachineIdentity, MachineKey};
    use shepherd_testkit::contracts::link::{Events, LinkFixture};
    use shepherd_testkit::contracts::peers::channel;
    use tempfile::TempDir;

    use super::*;
    use crate::identity::KeyFileIdentity;
    use crate::peers::{Discovery, IrohPeers};
    use shepherd_core::ports::Peers;

    /// A real node id, of a key nobody binds.
    fn nobody() -> MachineId {
        MachineId::new(iroh::SecretKey::from_bytes(&[7; 32]).public().to_string())
    }

    /// A machine on localhost: its port, bound, with relays and address lookup off (no network).
    struct Node {
        peers: IrohPeers,
        id: MachineId,
    }

    /// A client machine's link, and which machine it is.
    struct Client {
        link: IrohLink,
        id: MachineId,
    }

    impl Link for Client {
        fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
            self.link.send(conn, line)
        }

        fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
            self.link.close(conn)
        }

        fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
            self.link.queued_bytes(conn)
        }

        fn flush_closed(&mut self, within: Duration) -> FlushOutcome {
            self.link.flush_closed(within)
        }

        fn peer(&self, conn: ConnId) -> Result<Option<MachineId>, LinkError> {
            self.link.peer(conn)
        }

        fn drained_bytes(&self, conn: ConnId) -> Option<u64> {
            self.link.drained_bytes(conn)
        }

        fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError> {
            self.link.pause_reading(conn, paused)
        }

        fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError> {
            self.link.open(peer)
        }
    }

    /// A host machine, and each client a machine of its own, every one with a fresh key in a root of its own in one
    /// temp dir. The machines' ports live as long as the fixture; dropping a link only ends its conns.
    struct LocalMachines {
        dir: TempDir,
        next: u32,
        host: Node,
        trusted: BTreeSet<MachineId>,
        clients: Vec<Node>,
    }

    impl LocalMachines {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut host = Self::node_in(dir.path().join("host"));
            // Trusted, so opening a conn to it dials (and finds nobody) rather than stopping at trust.
            let trusted = BTreeSet::from([nobody()]);
            host.peers.set_trusted(&trusted);
            Self {
                dir,
                next: 0,
                host,
                trusted,
                clients: Vec::new(),
            }
        }

        fn node_in(root: PathBuf) -> Node {
            let key = KeyFileIdentity::new(&root).load_or_create().unwrap();
            let (sink, _events, _paths) = channel();
            let mut peers = IrohPeers::new(Some(&key), Discovery::Local, None, sink).unwrap();
            peers.bind();
            Node {
                peers,
                id: key.machine_id(),
            }
        }

        /// A fresh client machine that knows where the host is and trusts it, trusted by the host if `trusted`, and
        /// its conn to the host, opened.
        fn client(&mut self, sink: Arc<dyn LinkEventSink>, trusted: bool) -> (Client, ConnId) {
            self.next += 1;
            let mut node = Self::node_in(self.dir.path().join(format!("client-{}", self.next)));
            if trusted {
                self.trusted.insert(node.id.clone());
                self.host.peers.set_trusted(&self.trusted);
            }
            // Answered once the host's runtime has taken the trusted set too: it takes calls in order.
            let addr = self.host.peers.addr().expect("the host is bound");
            node.peers.remember(addr);
            node.peers
                .set_trusted(&BTreeSet::from([self.host.id.clone()]));
            let mut link = node.peers.link(sink);
            let conn = link.open(&self.host.id).unwrap();
            let client = Client {
                link,
                id: node.id.clone(),
            };
            self.clients.push(node);
            (client, conn)
        }
    }

    impl LinkFixture for LocalMachines {
        type Host = IrohLink;
        type Client = Client;
        type Stalled = Client;

        fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> IrohLink {
            self.host.peers.link(sink)
        }

        fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (Client, ConnId) {
            self.client(sink, true)
        }

        /// Paused before it reads anything, and announced on both ends before it is handed out, so even one dropped
        /// at once was announced by the host.
        fn stalled_peer(&mut self) -> Client {
            let (sink, events) = Events::channel();
            let (mut client, conn) = self.client(sink, true);
            client.pause_reading(conn, true).unwrap();
            events.wait_for(
                |e| matches!(e, LinkEvent::Connected { conn: c } if *c == conn),
                Duration::from_secs(10),
            );
            client
        }

        fn machine_of(&self, client: &Client) -> Option<MachineId> {
            Some(client.id.clone())
        }

        fn host_machine(&self) -> Option<MachineId> {
            Some(self.host.id.clone())
        }

        fn untrusted(&mut self, sink: Arc<dyn LinkEventSink>) -> Option<(Client, ConnId)> {
            Some(self.client(sink, false))
        }

        fn nobody(&self) -> MachineId {
            nobody()
        }
    }

    mod contract {
        use super::LocalMachines;

        shepherd_testkit::link_contract!(LocalMachines::new());
    }

    #[test]
    fn a_conn_to_what_is_not_a_machine_id_ends_as_peer_gone() {
        let machines = LocalMachines::new();
        let (sink, events) = Events::channel();
        let mut link = machines.host.peers.link(sink);
        let nobody = MachineId::new("not-a-machine");
        let conn = link.open(&nobody).unwrap();
        assert_eq!(
            events.disconnect_of(conn),
            (vec![], DisconnectReason::PeerGone)
        );
    }

    /// A host refuses a tunnel past [`MAX_OPEN_PER_PEER`] to one machine; up to that many open at once over its one
    /// connection, none waiting for another to close.
    #[test]
    fn as_many_conns_as_a_host_allows_open_at_once_to_one_peer() {
        let mut machines = LocalMachines::new();
        let (host_sink, host_events) = Events::channel();
        let _host = machines.host.peers.link(host_sink);
        let (sink, events) = Events::channel();
        let (mut client, first) = machines.client(sink, true);
        let connected = |events: &Events| {
            events.wait_for(
                |e| matches!(e, LinkEvent::Connected { .. }),
                Duration::from_secs(30),
            );
        };
        // The first dials the connection; the rest are streams on it.
        connected(&events);
        let host = machines.host.id.clone();
        let rest: Vec<ConnId> = (1..MAX_OPEN_PER_PEER)
            .map(|_| client.open(&host).unwrap())
            .collect();
        for _ in 1..MAX_OPEN_PER_PEER {
            connected(&events);
        }
        for _ in 0..MAX_OPEN_PER_PEER {
            connected(&host_events);
        }
        assert_eq!(rest.len() + 1, MAX_OPEN_PER_PEER);
        assert_ne!(first, rest[0]);
    }

    #[test]
    fn a_conn_on_a_dropped_connection_ends_as_peer_gone() {
        let mut machines = LocalMachines::new();
        let (host_sink, host_events) = Events::channel();
        let _host = machines.host.peers.link(host_sink);
        let (sink, events) = Events::channel();
        let (_client, conn) = machines.client(sink, true);
        host_events.wait_for(
            |e| matches!(e, LinkEvent::Connected { .. }),
            Duration::from_secs(10),
        );
        // The host stops trusting it: its connections go at once.
        machines.host.peers.set_trusted(&BTreeSet::new());
        assert_eq!(events.disconnect_of(conn).1, DisconnectReason::PeerGone);
    }
}
