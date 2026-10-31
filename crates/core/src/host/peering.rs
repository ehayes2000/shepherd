//! Pairing and trusted peers (T30, decision 0019).
//!
//! Who this machine trusts is a fold of the host log: `_shepherd/peer/trusted` trusts a machine (a later one renames
//! it) and `_shepherd/peer/removed` stops trusting it. The set is pushed to the [`Peers`](crate::ports::Peers) port as
//! data, and the port refuses every other machine. Machine ids are compared as the port gives them: canonical node
//! ids, never parsed here.
//!
//! The endpoint is bound lazily: only while this machine trusts someone, has a pairing open, or is joining one. A host
//! that never paired never binds, so it makes no network calls. Binding (on start, with peers already trusted) also
//! dials each trusted peer once, so machines that are both up find each other.
//!
//! Inviting (`shepherd pair`): `_shepherd/pair/start` opens one pairing with a token the runtime made from OS
//! randomness ([`PeeringEvent::Start`]), replacing any open one, and is answered with its ticket once the port has it.
//! The pairing expires [`PAIRING_TTL`] after it started (on the first [`PeeringEvent::Tick`] at or past that). The
//! first attempt uses it up: the port checks its token and says whether it matched ([`PeersEvent::PairRequested`]);
//! a match is recorded in `peer/trusted`. `_shepherd/pair/wait` is answered when it ends, however it ends. Once every
//! client that was waiting on it (its start, its waits) has gone ([`PeeringEvent::Gone`]: `shepherd pair` stopped with
//! Ctrl-C), it is stopped.
//!
//! Joining (`shepherd join <ticket>`): `_shepherd/pair/join` dials the inviter through the port, and once the inviter
//! accepted, records it in `peer/trusted` and answers. Names peers give are display-safe before they are recorded
//! ([`crate::machine::peer_name`]).
//!
//! Reconnecting: a trusted peer whose connection goes ([`PeersEvent::Disconnected`]) is dialled again
//! [`RECONNECT_MIN`] later, and each dial that can't reach it ([`ConnectError::Unreachable`]) doubles the wait, up to
//! [`RECONNECT_MAX`]; a connection starts the waits over. A peer that refuses this machine isn't dialled again (until
//! the endpoint is bound again), nor is anyone once the endpoint is unbound.
//!
//! Tunnels (`_shepherd/tunnel`, T31a): one of this machine's own clients may make its conn, with its first request,
//! a tunnel to the host of a machine this one trusts ([`Peering::tunnel`]); the runtime splices it. Why one couldn't
//! open is said in the peer's name ([`Peering::tunnel_failed`]).
//!
//! Not all of this is host state: a pairing, the joins in flight which peers are connected and when to dial each again
//! are known only to this host run. The runtime keeps one [`Peering`], applies every entry it appends (and replays the logs into it on start),
//! hands it the port's events and the requests, services its [`Peering::deadline`] with ticks stamped with the clock,
//! and after executing each batch of its commands executes [`Peering::sync`], which pushes what changed.

use std::collections::{BTreeMap, BTreeSet};

use jiff::SignedDuration;

use super::{HostCommand, ReplyTo, append_host};
use crate::acpp::{Envelope, Frame, PeerRemoved, PeerTrusted, ShepherdFrame, Timestamp};
use crate::ids::MachineId;
use crate::machine::{UNNAMED, peer_name};
use crate::ports::{ConnectError, DisconnectReason, PairTicket, PairToken, PairingId, PeersEvent};
use crate::wire::{HostResult, PairOutcome, PairStartOk, PeerInfo, PeerListOk, WireError};

/// How long a pairing stays open.
pub const PAIRING_TTL: SignedDuration = SignedDuration::from_mins(10);
/// How long after a trusted peer's connection goes it is dialled again.
pub const RECONNECT_MIN: SignedDuration = SignedDuration::from_secs(1);
/// The longest wait between dials of a trusted peer that can't be reached.
pub const RECONNECT_MAX: SignedDuration = SignedDuration::from_mins(1);

/// The requests, time passing, and what the `Peers` port reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeeringEvent {
    /// `_shepherd/pair/start` at `at`, with a fresh token from the runtime.
    Start {
        reply: ReplyTo,
        token: PairToken,
        at: Timestamp,
    },
    /// `_shepherd/pair/wait`.
    Wait { reply: ReplyTo },
    /// `_shepherd/pair/join`.
    Join { reply: ReplyTo, ticket: PairTicket },
    /// `_shepherd/peer/list`.
    List { reply: ReplyTo },
    /// `_shepherd/peer/remove`, naming the peer by id or name.
    Remove { reply: ReplyTo, peer: String },
    /// It is `at`: the deadline the runtime scheduled, or later.
    Tick { at: Timestamp },
    /// The client `reply` was to answer went away (`shepherd pair` stopped with Ctrl-C).
    Gone { reply: ReplyTo },
    /// What the port reported, at `at`.
    Peers { event: PeersEvent, at: Timestamp },
}

/// A call to the `Peers` port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeersCommand {
    Bind,
    Unbind,
    SetTrusted(BTreeSet<MachineId>),
    AcceptPairing {
        id: PairingId,
        token: PairToken,
        name: String,
    },
    StopPairing,
    Join {
        id: PairingId,
        ticket: PairTicket,
        name: String,
    },
    Connect {
        peer: MachineId,
    },
}

/// Trusted peers and pairing. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peering {
    /// Trusted machines and what they call themselves (host log).
    trusted: BTreeMap<MachineId, String>,
    /// What this machine was last recorded as being called (`machine/renamed`).
    name: Option<String>,
    /// The last [`PairingId`] given out.
    last_id: u64,
    pending: Option<Pending>,
    /// Joins in flight, with the request each answers.
    joins: BTreeMap<PairingId, ReplyTo>,
    /// Trusted machines with a connection now.
    connected: BTreeSet<MachineId>,
    /// The port was last asked to bind (true) or unbind.
    bound: bool,
    /// The trusted set the port was last given (it starts with none).
    pushed: BTreeSet<MachineId>,
    /// Trusted peers to dial again, by how many dials in a row failed.
    redial: BTreeMap<MachineId, Redial>,
}

/// A trusted peer to dial again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Redial {
    /// Dials in a row that couldn't reach it.
    failures: u32,
    /// When to dial it; `None` while a dial is on its way.
    due: Option<jiff::Timestamp>,
}

/// The open pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: PairingId,
    expires: Timestamp,
    /// The start, until its ticket answers it.
    start: Option<ReplyTo>,
    waiters: Vec<ReplyTo>,
}

impl Peering {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one log entry: `peer/trusted`, `peer/removed` and `machine/renamed` in the host log.
    pub fn apply(&mut self, env: &Envelope) {
        let (None, Frame::Shepherd(m)) = (env.session(), env.frame()) else {
            return;
        };
        match m.body() {
            ShepherdFrame::PeerTrusted(trusted) => {
                self.trusted
                    .insert(trusted.node.clone(), trusted.name.clone());
            }
            ShepherdFrame::PeerRemoved(removed) => {
                self.trusted.remove(&removed.node);
                self.connected.remove(&removed.node);
                self.redial.remove(&removed.node);
            }
            ShepherdFrame::MachineRenamed(renamed) => self.name = Some(renamed.name.clone()),
            _ => {}
        }
    }

    /// Decides what to do about `event`. Binding (and pushing the trusted set first) comes before the commands that
    /// need the endpoint. What the appends change, and unbinding once nothing needs the endpoint, is pushed by
    /// [`Peering::sync`] once they are applied.
    pub fn handle(&mut self, event: PeeringEvent) -> Vec<HostCommand> {
        let commands = match event {
            PeeringEvent::Start { reply, token, at } => self.start(reply, token, at),
            PeeringEvent::Wait { reply } => match &mut self.pending {
                Some(pending) => {
                    pending.waiters.push(reply);
                    Vec::new()
                }
                None => vec![invalid(
                    reply,
                    "no pairing is open: start one with `shepherd pair`",
                )],
            },
            PeeringEvent::Join { reply, ticket } => {
                let id = self.next_id();
                self.joins.insert(id, reply);
                vec![peers(PeersCommand::Join {
                    id,
                    ticket,
                    name: self.my_name(),
                })]
            }
            PeeringEvent::List { reply } => {
                let mut list: Vec<PeerInfo> = self.trusted.keys().map(|id| self.info(id)).collect();
                list.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
                vec![ok(reply, HostResult::PeerList(PeerListOk { peers: list }))]
            }
            PeeringEvent::Remove { reply, peer } => self.remove(reply, &peer),
            PeeringEvent::Tick { at } => self.tick(&at),
            PeeringEvent::Gone { reply } => self.gone(reply),
            PeeringEvent::Peers { event, at } => self.port_event(event, &at),
        };
        let mut out = self.bind_if_wanted();
        out.extend(commands);
        out
    }

    /// What changed since the port was last told, to push now that the last batch's appends are applied: the trusted
    /// set, and binding (dialling each trusted peer) or unbinding. Nothing when nothing changed.
    pub fn sync(&mut self) -> Vec<HostCommand> {
        let mut out = self.bind_if_wanted();
        if !self.wanted() && self.bound {
            self.bound = false;
            self.connected.clear();
            self.redial.clear();
            out.push(peers(PeersCommand::Unbind));
        }
        out
    }

    /// When the open pairing expires or a peer is to be dialled again, whichever comes first: when to send the next
    /// [`PeeringEvent::Tick`].
    pub fn deadline(&self) -> Option<jiff::Timestamp> {
        let expiry = self.pending.as_ref().map(|p| p.expires.instant());
        let redials = self.redial.values().filter_map(|r| r.due);
        expiry.into_iter().chain(redials).min()
    }

    /// Whether a conn may become a tunnel to `machine`'s host: it must be from one of this machine's own clients
    /// (`from` is `None`: not a peer's conn), this must be its first request, and this machine must trust `machine`.
    pub fn tunnel(
        &self,
        machine: &MachineId,
        from: Option<&MachineId>,
        first: bool,
    ) -> Result<(), WireError> {
        let refuse = |why: &str| Err(WireError::InvalidParams(why.to_owned()));
        if from.is_some() {
            return refuse("a tunnel can only be opened by this machine's own clients");
        }
        if !first {
            return refuse("a tunnel must be the first request on its connection");
        }
        if !self.trusted.contains_key(machine) {
            return refuse("no trusted machine has that id: pair with it first (`shepherd pair`)");
        }
        Ok(())
    }

    /// Why a tunnel to `machine` couldn't open, given how its conn to that host ended before it was accepted.
    pub fn tunnel_failed(&self, machine: &MachineId, reason: &DisconnectReason) -> WireError {
        let name = self
            .trusted
            .get(machine)
            .cloned()
            .unwrap_or_else(|| machine.to_string());
        WireError::InvalidParams(match reason {
            DisconnectReason::Refused => {
                format!("{name} doesn't trust this machine: pair them again with `shepherd pair`")
            }
            _ => format!("cannot reach {name}"),
        })
    }

    /// The endpoint is needed: something is trusted, a pairing is open, or a join is in flight.
    fn wanted(&self) -> bool {
        !self.trusted.is_empty() || self.pending.is_some() || !self.joins.is_empty()
    }

    /// The trusted set if it changed, then binding and dialling each trusted peer if the endpoint is wanted and not
    /// bound.
    fn bind_if_wanted(&mut self) -> Vec<HostCommand> {
        let mut out = Vec::new();
        let trusted: BTreeSet<MachineId> = self.trusted.keys().cloned().collect();
        if trusted != self.pushed {
            out.push(peers(PeersCommand::SetTrusted(trusted.clone())));
            self.pushed = trusted.clone();
        }
        if self.wanted() && !self.bound {
            self.bound = true;
            out.push(peers(PeersCommand::Bind));
            out.extend(
                trusted
                    .into_iter()
                    .map(|peer| peers(PeersCommand::Connect { peer })),
            );
        }
        out
    }

    fn start(&mut self, reply: ReplyTo, token: PairToken, at: Timestamp) -> Vec<HostCommand> {
        let mut commands = Vec::new();
        if let Some(old) = self.pending.take() {
            if let Some(start) = old.start {
                commands.push(invalid(start, "replaced by a newer `shepherd pair`"));
            }
            commands.extend(
                old.waiters
                    .into_iter()
                    .map(|to| ok(to, HostResult::PairWait(PairOutcome::Replaced))),
            );
        }
        let id = self.next_id();
        let expires = at
            .instant()
            .checked_add(PAIRING_TTL)
            .ok()
            .and_then(|instant| Timestamp::parse(instant.to_string()).ok())
            .unwrap_or(at);
        self.pending = Some(Pending {
            id,
            expires,
            start: Some(reply),
            waiters: Vec::new(),
        });
        commands.push(peers(PeersCommand::AcceptPairing {
            id,
            token,
            name: self.my_name(),
        }));
        commands
    }

    fn tick(&mut self, at: &Timestamp) -> Vec<HostCommand> {
        let mut commands = Vec::new();
        for (peer, redial) in &mut self.redial {
            if redial.due.is_some_and(|due| due <= at.instant()) {
                redial.due = None;
                commands.push(peers(PeersCommand::Connect { peer: peer.clone() }));
            }
        }
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|p| p.expires.instant() <= at.instant());
        if expired {
            commands.push(peers(PeersCommand::StopPairing));
            commands.extend(self.end_pending(|| PairOutcome::Expired, "the pairing expired"));
        }
        commands
    }

    /// Dials trusted `peer` again after `failures` dials in a row couldn't reach it: [`RECONNECT_MIN`] from `at`,
    /// doubled for each failure, at most [`RECONNECT_MAX`]. Nothing while the endpoint isn't bound.
    fn redial(&mut self, peer: MachineId, failures: u32, at: &Timestamp) {
        if !self.bound || !self.trusted.contains_key(&peer) {
            return;
        }
        let wait = (0..failures)
            .try_fold(RECONNECT_MIN, |wait, _| wait.checked_mul(2))
            .unwrap_or(RECONNECT_MAX)
            .min(RECONNECT_MAX);
        // A wait past the end of time (it can't be) dials at once rather than never.
        let now = at.instant();
        let due = Some(now.checked_add(wait).unwrap_or(now));
        self.redial.insert(peer, Redial { failures, due });
    }

    /// Forgets `reply`; once nobody is left to tell how the open pairing ends, it is stopped.
    fn gone(&mut self, reply: ReplyTo) -> Vec<HostCommand> {
        let Some(pending) = &mut self.pending else {
            return Vec::new();
        };
        let before = pending.waiters.len();
        pending.waiters.retain(|to| *to != reply);
        let held = pending.waiters.len() < before || pending.start == Some(reply);
        if pending.start == Some(reply) {
            pending.start = None;
        }
        // Kept while its start (its ticket not shown yet) or a waiter is still there. Once the start is answered, a
        // client going between that answer and its wait leaves nobody holding the pairing, so it is stopped. With two
        // clients this can race: client A's start is answered, client B's wait goes before A's wait arrives, and B
        // leaving stops A's pairing (A's wait is then told no pairing is open). Only a request this pairing holds
        // stops it: some other request going leaves it alone.
        if !held || pending.start.is_some() || !pending.waiters.is_empty() {
            return Vec::new();
        }
        self.pending = None;
        vec![peers(PeersCommand::StopPairing)]
    }

    fn remove(&mut self, reply: ReplyTo, who: &str) -> Vec<HostCommand> {
        let by_id = self.trusted.keys().find(|id| id.as_str() == who).cloned();
        let id = match by_id {
            Some(id) => id,
            None => {
                let named: Vec<&MachineId> = self
                    .trusted
                    .iter()
                    .filter(|(_, name)| name.as_str() == who)
                    .map(|(id, _)| id)
                    .collect();
                match named.as_slice() {
                    [id] => (*id).clone(),
                    [] => return vec![invalid(reply, "no trusted machine has that name or id")],
                    _ => {
                        return vec![invalid(
                            reply,
                            "more than one trusted machine has that name: unpair it by id",
                        )];
                    }
                }
            }
        };
        let removed = PeerInfo {
            connected: false,
            ..self.info(&id)
        };
        vec![
            append_host(ShepherdFrame::PeerRemoved(PeerRemoved { node: id })),
            ok(reply, HostResult::PeerRemove(removed)),
        ]
    }

    fn port_event(&mut self, event: PeersEvent, at: &Timestamp) -> Vec<HostCommand> {
        match event {
            PeersEvent::PairingOpen { id, ticket } => match &mut self.pending {
                Some(pending) if pending.id == id => match pending.start.take() {
                    Some(start) => vec![ok(
                        start,
                        HostResult::PairStart(PairStartOk {
                            ticket: ticket.as_str().to_owned(),
                            expires: pending.expires.clone(),
                        }),
                    )],
                    None => Vec::new(),
                },
                _ => Vec::new(),
            },
            PeersEvent::PairingFailed { id, error } => {
                if !self.is_pending(id) {
                    return Vec::new();
                }
                let why = format!("cannot open a pairing: {error}");
                let pending = self.pending.take();
                let mut commands = Vec::new();
                if let Some(Pending { start, waiters, .. }) = pending {
                    commands.extend(start.map(|to| internal(to, &why)));
                    commands.extend(waiters.into_iter().map(|to| internal(to, &why)));
                }
                commands
            }
            PeersEvent::PairRequested {
                id,
                peer,
                name,
                accepted,
            } => {
                let name = peer_name(&name);
                let mut commands = Vec::new();
                // A match on a pairing replaced a moment before still pairs: the user handed that ticket out.
                if accepted {
                    commands.push(trust(&peer, &name));
                }
                if self.is_pending(id) {
                    let info = PeerInfo {
                        id: peer.clone(),
                        name: name.clone(),
                        connected: self.connected.contains(&peer),
                    };
                    let outcome = move || match accepted {
                        true => PairOutcome::Paired { peer: info.clone() },
                        false => PairOutcome::Refused {
                            peer: info.id.clone(),
                            name: info.name.clone(),
                        },
                    };
                    commands.extend(self.end_pending(outcome, "the pairing was used up"));
                }
                commands
            }
            PeersEvent::Paired { id, peer, name } => {
                // Only an answer to a join this host asked for trusts anyone.
                let Some(to) = self.joins.remove(&id) else {
                    return Vec::new();
                };
                let name = peer_name(&name);
                let info = PeerInfo {
                    connected: self.connected.contains(&peer),
                    id: peer.clone(),
                    name: name.clone(),
                };
                vec![trust(&peer, &name), ok(to, HostResult::PairJoin(info))]
            }
            PeersEvent::JoinFailed { id, error } => match self.joins.remove(&id) {
                Some(to) => vec![invalid(to, &error.to_string())],
                None => Vec::new(),
            },
            PeersEvent::Connected { peer } => {
                self.redial.remove(&peer);
                if self.trusted.contains_key(&peer) {
                    self.connected.insert(peer);
                }
                Vec::new()
            }
            PeersEvent::Disconnected { peer } => {
                self.connected.remove(&peer);
                self.redial(peer, 0, at);
                Vec::new()
            }
            PeersEvent::ConnectFailed {
                peer,
                error: ConnectError::Unreachable(_),
            } => {
                let failures = self.redial.get(&peer).map_or(0, |r| r.failures);
                self.redial(peer, failures.saturating_add(1), at);
                Vec::new()
            }
            // A peer that refuses this machine, or a dial with no endpoint: dialling again changes nothing.
            PeersEvent::ConnectFailed { peer, .. } => {
                self.redial.remove(&peer);
                Vec::new()
            }
            // The port already failed or refused it; nothing follows from it.
            PeersEvent::BindFailed { .. } | PeersEvent::Refused { .. } => Vec::new(),
        }
    }

    /// Ends the open pairing: its start (if its ticket never came) is answered with `why`, its waiters with
    /// `outcome`.
    fn end_pending(&mut self, outcome: impl Fn() -> PairOutcome, why: &str) -> Vec<HostCommand> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        let start = pending.start.map(|to| invalid(to, why));
        start
            .into_iter()
            .chain(
                pending
                    .waiters
                    .into_iter()
                    .map(|to| ok(to, HostResult::PairWait(outcome()))),
            )
            .collect()
    }

    fn is_pending(&self, id: PairingId) -> bool {
        self.pending.as_ref().is_some_and(|p| p.id == id)
    }

    fn next_id(&mut self) -> PairingId {
        self.last_id += 1;
        PairingId::new(self.last_id)
    }

    fn my_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| UNNAMED.to_owned())
    }

    fn info(&self, id: &MachineId) -> PeerInfo {
        PeerInfo {
            id: id.clone(),
            name: self.trusted.get(id).cloned().unwrap_or_default(),
            connected: self.connected.contains(id),
        }
    }
}

fn peers(command: PeersCommand) -> HostCommand {
    HostCommand::Peers(command)
}

fn trust(peer: &MachineId, name: &str) -> HostCommand {
    append_host(ShepherdFrame::PeerTrusted(PeerTrusted {
        node: peer.clone(),
        name: name.to_owned(),
    }))
}

fn ok(to: ReplyTo, result: HostResult) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Ok(result),
    }
}

fn invalid(to: ReplyTo, why: &str) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Err(WireError::InvalidParams(why.to_owned())),
    }
}

fn internal(to: ReplyTo, why: &str) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Err(WireError::Internal(why.to_owned())),
    }
}
