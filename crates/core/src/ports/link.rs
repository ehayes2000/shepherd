//! Moving wire messages between a client and a host, independent of transport. The trait moves **lines**: one
//! encoded wire message each, without its `\n`. Framing (splitting the byte stream with
//! [`Decoder`](crate::wire::Decoder)) happens inside the adapter.
//!
//! Like [`Pty`](super::Pty), the trait is synchronous and pushes what happens to the [`LinkEventSink`] an
//! implementation is built with. A host-side link emits `Connected` for each client that connects. A client-side
//! link learns its single conn from the constructor and emits no `Connected`.
//!
//! Semantics every implementation keeps:
//! - Lines arrive in the order they were sent, per conn and direction.
//! - Each conn gets exactly one `Disconnected`, after all its `Line`s. Once it is sent, or once [`Link::close`]
//!   returns, the conn is unknown to the link.
//! - [`Link::send`] never blocks on a slow peer. Each conn has an outbound queue. A conn whose unsent backlog
//!   would grow past [`MAX_QUEUED`] bytes is disconnected with [`DisconnectReason::SlowConsumer`]. A single line
//!   into an empty queue is always accepted, so lines up to the decoder limit can be sent.
//! - A peer that sends a line over [`MAX_LINE`](crate::wire::MAX_LINE) is disconnected with
//!   [`DisconnectReason::Protocol`].
//! - `close` delivers the lines already queued, then disconnects: the closing end sees `Closed`, the other end
//!   `PeerGone`. Once `close` returns, no more `Line`s are emitted for that conn.
//! - Delivering what a closed conn had queued happens in the background, for up to [`CLOSE_FLUSH`] on a peer that
//!   isn't reading. [`Link::flush_closed`] waits for it, so a process about to exec (decision 0018) doesn't cut it off.
//! - Dropping a link disconnects all its conns without flushing; their peers see `PeerGone`.
//! - While a conn's reading is [paused](Link::pause_reading), none of its `Line`s are emitted: what its peer sends
//!   waits in the transport, so the peer's own queue grows (back-pressure). Resuming emits them, in order.
//!
//! Links between machines (decision 0019) also [open](Link::open) conns to another machine's host. Such a conn is
//! announced with `Connected` once that host accepted it, and each end knows which machine the other is
//! ([`Link::peer`]). A host that doesn't trust this machine refuses it: the conn ends with
//! [`DisconnectReason::Refused`] and that host never announces it.

use std::fmt;
use std::time::Duration;

use crate::ids::MachineId;

/// The most unsent bytes a conn may have queued before it is disconnected as a slow consumer.
pub const MAX_QUEUED: usize = 8 * 1024 * 1024;

/// The most conns a link between machines carries open at once to one peer, of those this end [opened](Link::open):
/// the concurrent stream limit of the one connection they share (QUIC's, on iroh, which each end grants the other).
/// Opening more would wait for one to close, so the host refuses to.
///
/// The count is of conns the link still knows, which is not quite the transport's: a closed conn's stream keeps its
/// place in QUIC's limit while it finishes delivering (up to [`CLOSE_FLUSH`]) and until the peer grants it back. So a
/// conn opened right after one closed, at the limit, can wait in the transport for a stream, and if none frees up in
/// time it ends as `PeerGone`, which a tunnel reports as "cannot reach" its machine. Accepted: it takes a hundred
/// tunnels to one machine at once.
pub const MAX_OPEN_PER_PEER: usize = 100;

/// How long a closed conn keeps trying to deliver its queued lines to a peer that isn't reading.
pub const CLOSE_FLUSH: Duration = Duration::from_secs(2);

/// One connection on a link. Assigned by the adapter, unique per adapter instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(u64);

impl ConnId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conn{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A client connected to a host-side link, or a conn this link [opened](Link::open) was accepted.
    Connected { conn: ConnId },
    /// One line from the peer, without its `\n`.
    Line { conn: ConnId, line: Vec<u8> },
    /// The last event for `conn`.
    Disconnected {
        conn: ConnId,
        reason: DisconnectReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// This end called [`Link::close`] or dropped the link.
    Closed,
    /// The peer closed, went away, or the transport failed.
    PeerGone,
    /// The peer didn't drain its queue; see [`MAX_QUEUED`].
    SlowConsumer,
    /// The peer broke framing, e.g. a line over the decoder limit.
    Protocol(String),
    /// The machine this end [opened](Link::open) a conn to doesn't trust this one.
    Refused,
}

/// Where a link pushes its events, from its own threads.
///
/// `send` must never block waiting on the link's owner (the code calling [`Link`] methods): an adapter may hold
/// a per-conn lock while it sends a `Line`, and [`Link::close`] takes that lock, so an owner that is blocked in
/// `close` while the sink waits for it would deadlock. Forward into an unbounded channel.
pub trait LinkEventSink: Send + Sync + 'static {
    fn send(&self, event: LinkEvent);
}

/// How [`Link::flush_closed`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Every conn this link closed is done: what was queued for it is written, or its peer left.
    Flushed,
    /// The deadline passed with some closed conn still delivering, or the link gave up on a closed conn's peer that
    /// wasn't reading (after [`CLOSE_FLUSH`]), so some of what was queued for it was never written. A give-up is
    /// reported to the next call, then forgotten: a conn given up on doesn't hold a later call.
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("unknown conn")]
    UnknownConn,
    #[error("line contains a newline")]
    InvalidLine,
    #[error("link io: {0}")]
    Io(String),
    /// This link reaches no other machines: it can't [open](Link::open) conns.
    #[error("this link reaches no other machines")]
    NoPeers,
}

pub trait Link {
    /// Queues `line` (without `\n`) for `conn`. Never blocks on a slow peer.
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError>;
    /// Flushes what is queued for `conn`, then disconnects it. Its `Disconnected` arrives through the sink.
    fn close(&mut self, conn: ConnId) -> Result<(), LinkError>;
    /// The bytes sent to `conn` that are still waiting in its outbound queue, or `None` if `conn` is unknown. It
    /// rises while the peer doesn't read and falls as it drains, so a sender can pace itself well below
    /// [`MAX_QUEUED`] (decision 0013).
    fn queued_bytes(&self, conn: ConnId) -> Option<usize>;
    /// Waits until every conn this link has [closed](Link::close) has finished delivering what was queued for it
    /// (written to the transport, or the conn is gone), or until `within` has passed, whichever comes first. The link
    /// times the wait on the clock it waits with. Blocks the caller: for a process about to exec, which would otherwise
    /// cut the deliveries off (decision 0018).
    fn flush_closed(&mut self, within: Duration) -> FlushOutcome;
    /// The machine `conn` is with, as the transport authenticated it, or `None` for a conn with this machine's own user
    /// (a local socket, which only that user can reach). A conn that is unknown (never was, or closed or gone) is
    /// [`LinkError::UnknownConn`], never `None`: a caller allowing something only to local conns must refuse it.
    fn peer(&self, conn: ConnId) -> Result<Option<MachineId>, LinkError>;
    /// How many bytes sent to `conn` the transport has delivered so far, counted as they are written, so one long line
    /// counts as it goes. It only rises, and it rises while the peer reads even when [`Link::queued_bytes`] doesn't
    /// fall (new lines keep coming, or one huge line is on its way): it tells a slow peer from one that stopped
    /// reading. `None` if `conn` is unknown.
    fn drained_bytes(&self, conn: ConnId) -> Option<u64>;
    /// Stops (`true`) or resumes (`false`) reading `conn`. See the module docs.
    fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError>;
    /// Opens a conn to the host of machine `peer` and returns it at once; lines sent to it before it is announced
    /// (`Connected`) wait in its queue. It ends with [`DisconnectReason::Refused`] if that host doesn't trust this
    /// machine, and with [`DisconnectReason::PeerGone`] if it can't be reached. A link that reaches no other machines
    /// returns [`LinkError::NoPeers`].
    fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError>;
}
