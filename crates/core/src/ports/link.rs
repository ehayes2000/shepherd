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

use std::fmt;
use std::time::Duration;

/// The most unsent bytes a conn may have queued before it is disconnected as a slow consumer.
pub const MAX_QUEUED: usize = 8 * 1024 * 1024;

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
    /// A client connected to a host-side link.
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
    /// Every conn this link closed is done: what was queued for it is written, or it is gone (its peer left, or the
    /// link gave up on a peer that wasn't reading). So a peer that never reads may still end as `Flushed`: the unix
    /// adapter's writer gives up on it after [`CLOSE_FLUSH`], usually just before a caller waiting as long times out.
    Flushed,
    /// The deadline passed with some closed conn still delivering.
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
}
