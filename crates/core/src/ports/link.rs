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
//!   `PeerGone`. No `Line`s are emitted for a conn after `close` is called on it.
//! - Dropping a link disconnects all its conns without flushing; their peers see `PeerGone`.

use std::fmt;

/// The most unsent bytes a conn may have queued before it is disconnected as a slow consumer.
pub const MAX_QUEUED: usize = 8 * 1024 * 1024;

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

pub trait LinkEventSink: Send + Sync + 'static {
    fn send(&self, event: LinkEvent);
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
}
