//! Reaching this machine's peers (T30, decision 0019): the host's one network endpoint, bound with the machine key
//! (T29), through which machines pair once and then connect, each refusing any machine it doesn't trust.
//!
//! Like [`Link`](super::Link), the trait is synchronous and never blocks: each call is queued, and what happens is
//! pushed to the [`PeersEventSink`] an implementation is built with, from its own threads. Calls take effect in the
//! order they were made.
//!
//! What core decides, and pushes as data: whether the endpoint is bound ([`Peers::bind`], [`Peers::unbind`]), which
//! machines are trusted ([`Peers::set_trusted`]), when pairing is open ([`Peers::accept_pairing`],
//! [`Peers::stop_pairing`]) and whom to dial ([`Peers::join`], [`Peers::connect`]). What every implementation enforces
//! by itself, mechanically:
//! - A connection from a machine outside the trusted set is refused once the remote is authenticated and before
//!   anything else happens on it: this end emits [`PeersEvent::Refused`] and never [`PeersEvent::Connected`] for it, and
//!   the dialler gets [`ConnectError::Refused`]. Taking a machine out of the set drops its connections at once.
//! - A pairing is single use. Pairing is only reachable while one is open, and the first attempt consumes it whether
//!   its token matches or not ([`PeersEvent::PairRequested`]), so a ticket can't be guessed at. Tokens are compared
//!   in constant time.
//! - Names a peer sends are its own claim, untrusted text: core makes them display-safe before recording them.

use std::collections::BTreeSet;
use std::fmt;

use crate::ids::MachineId;

/// How many bytes a [`PairToken`] has.
pub const TOKEN_LEN: usize = 16;

/// The secret in a pairing ticket: 16 bytes of OS randomness, made by the runtime and handed to core in an event.
///
/// Its `Debug` doesn't show it. Equality is only for tests comparing commands: an attempt's token is checked by the
/// adapter, in constant time, never by comparing these.
#[derive(Clone, PartialEq, Eq)]
pub struct PairToken([u8; TOKEN_LEN]);

impl PairToken {
    pub fn new(bytes: [u8; TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self.0
    }
}

impl fmt::Debug for PairToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairToken(..)")
    }
}

/// A pairing ticket as text: what `shepherd pair` prints and `shepherd join` takes. Its format is the adapter's
/// (where the inviting machine is, its name, and the token); core never looks inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairTicket(String);

impl PairTicket {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names one pairing core opened ([`Peers::accept_pairing`]) or one join it asked for ([`Peers::join`]), so an event
/// about an older one is told apart from one about the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PairingId(u64);

impl PairingId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeersEvent {
    /// The endpoint couldn't be bound. Pairings and joins waiting on it fail with
    /// [`PairingError::Unbound`]/[`JoinError::Unbound`] events of their own.
    BindFailed { error: String },
    /// Pairing `id` is open: `ticket` reaches it.
    PairingOpen { id: PairingId, ticket: PairTicket },
    /// Pairing `id` couldn't be opened.
    PairingFailed { id: PairingId, error: PairingError },
    /// Someone used pairing `id`, which is consumed: `peer` (authenticated) calls itself `name`, and its token matched
    /// (`accepted`, and it was told this machine's name) or didn't (it was refused).
    PairRequested {
        id: PairingId,
        peer: MachineId,
        name: String,
        accepted: bool,
    },
    /// Join `id` worked: the inviting machine `peer` accepted this one, and calls itself `name`.
    Paired {
        id: PairingId,
        peer: MachineId,
        name: String,
    },
    /// Join `id` failed.
    JoinFailed { id: PairingId, error: JoinError },
    /// `peer`, a trusted machine, is connected: its first connection, either way, is up.
    Connected { peer: MachineId },
    /// Dialling `peer` ([`Peers::connect`]) failed.
    ConnectFailed {
        peer: MachineId,
        error: ConnectError,
    },
    /// `peer`'s last connection is gone: it closed, went away, was untrusted, or the endpoint was unbound.
    Disconnected { peer: MachineId },
    /// `peer`, a machine outside the trusted set, tried to connect and was refused.
    Refused { peer: MachineId },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("the network endpoint isn't bound")]
    Unbound,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The text isn't a pairing ticket.
    #[error("that is not a shepherd pairing ticket")]
    BadTicket,
    /// The inviting machine refused the ticket's token: it isn't the open pairing's (a newer one replaced it). That
    /// used the pairing up.
    #[error("the other machine refused the ticket: a newer `shepherd pair` replaced it")]
    Refused,
    /// The inviting machine has no pairing open: the ticket was already used, expired, or was stopped.
    #[error("the other machine has no pairing open: the ticket was already used, or it expired")]
    NotOpen,
    /// The inviting machine couldn't be reached.
    #[error("cannot reach the other machine: {0}")]
    Unreachable(String),
    #[error("the network endpoint isn't bound")]
    Unbound,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    /// The peer doesn't trust this machine.
    #[error("the other machine doesn't trust this one")]
    Refused,
    #[error("cannot reach the other machine: {0}")]
    Unreachable(String),
    #[error("the network endpoint isn't bound")]
    Unbound,
    /// This machine doesn't trust the peer, so it holds no connection with it.
    #[error("this machine doesn't trust the other one")]
    Untrusted,
}

/// Where a [`Peers`] implementation pushes its events, from its own threads. Like
/// [`LinkEventSink`](super::LinkEventSink), `send` must never block on the port's owner: forward into an unbounded
/// channel.
pub trait PeersEventSink: Send + Sync + 'static {
    fn send(&self, event: PeersEvent);
}

pub trait Peers {
    /// Binds the endpoint with the machine key the implementation was built with, so peers can reach this machine.
    /// Nothing happens if it is bound already.
    fn bind(&mut self);
    /// Closes the endpoint: every connection goes ([`PeersEvent::Disconnected`]), and an open pairing with it.
    /// Nothing happens if it isn't bound.
    fn unbind(&mut self);
    /// The machines allowed to connect from now on, replacing the set before. Connections of machines left out are
    /// dropped at once.
    fn set_trusted(&mut self, peers: &BTreeSet<MachineId>);
    /// Opens pairing `id`, replacing any open one: the first attempt with `token` is accepted and told this machine
    /// is called `name`. Answered with [`PeersEvent::PairingOpen`] (its ticket) or [`PeersEvent::PairingFailed`].
    fn accept_pairing(&mut self, id: PairingId, token: PairToken, name: &str);
    /// Closes the open pairing, if any.
    fn stop_pairing(&mut self);
    /// Join `id`: dials the machine `ticket` names and offers its token, saying this machine is called `name`.
    /// Answered with [`PeersEvent::Paired`] or [`PeersEvent::JoinFailed`].
    fn join(&mut self, id: PairingId, ticket: &PairTicket, name: &str);
    /// Dials `peer`, at an address it learned from pairing or from earlier connections, or found by looking its id
    /// up. Answered with [`PeersEvent::Connected`] or [`PeersEvent::ConnectFailed`]. Connections are held only between
    /// machines that trust each other: dialling a machine this one doesn't trust fails ([`ConnectError::Untrusted`]).
    fn connect(&mut self, peer: &MachineId);
}
