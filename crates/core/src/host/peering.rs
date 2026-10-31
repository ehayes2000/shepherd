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
//! a match is recorded in `peer/trusted`. `_shepherd/pair/wait` is answered when it ends, however it ends.
//!
//! Joining (`shepherd join <ticket>`): `_shepherd/pair/join` dials the inviter through the port, and once the inviter
//! accepted, records it in `peer/trusted` and answers. Names peers give are display-safe before they are recorded
//! ([`crate::machine::peer_name`]).
//!
//! Not all of this is host state: a pairing, the joins in flight and which peers are connected are known only to this
//! host run. The runtime keeps one [`Peering`], applies every entry it appends (and replays the logs into it on start),
//! hands it the port's events and the requests, services its [`Peering::deadline`] with ticks stamped with the clock,
//! and after executing each batch of its commands executes [`Peering::sync`], which pushes what changed.

use std::collections::{BTreeMap, BTreeSet};

use jiff::SignedDuration;

use super::{HostCommand, ReplyTo, append_host};
use crate::acpp::{Envelope, Frame, PeerRemoved, PeerTrusted, ShepherdFrame, Timestamp};
use crate::ids::MachineId;
use crate::machine::{UNNAMED, peer_name};
use crate::ports::{PairTicket, PairToken, PairingId, PeersEvent};
use crate::wire::{HostResult, PairOutcome, PairStartOk, PeerInfo, PeerListOk, WireError};

/// How long a pairing stays open.
pub const PAIRING_TTL: SignedDuration = SignedDuration::from_mins(10);

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
    Wait {
        reply: ReplyTo,
    },
    /// `_shepherd/pair/join`.
    Join {
        reply: ReplyTo,
        ticket: PairTicket,
    },
    /// `_shepherd/peer/list`.
    List {
        reply: ReplyTo,
    },
    /// `_shepherd/peer/remove`, naming the peer by id or name.
    Remove {
        reply: ReplyTo,
        peer: String,
    },
    /// It is `at`: the deadline the runtime scheduled, or later.
    Tick {
        at: Timestamp,
    },
    Peers(PeersEvent),
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
            PeeringEvent::Peers(event) => self.port_event(event),
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
            out.push(peers(PeersCommand::Unbind));
        }
        out
    }

    /// When the open pairing expires, if one is open: when to send the next [`PeeringEvent::Tick`].
    pub fn deadline(&self) -> Option<jiff::Timestamp> {
        self.pending.as_ref().map(|p| p.expires.instant())
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
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|p| p.expires.instant() <= at.instant());
        if !expired {
            return Vec::new();
        }
        let mut commands = vec![peers(PeersCommand::StopPairing)];
        commands.extend(self.end_pending(|| PairOutcome::Expired, "the pairing expired"));
        commands
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

    fn port_event(&mut self, event: PeersEvent) -> Vec<HostCommand> {
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
                let name = peer_name(&name);
                let mut commands = vec![trust(&peer, &name)];
                if let Some(to) = self.joins.remove(&id) {
                    let info = PeerInfo {
                        connected: self.connected.contains(&peer),
                        id: peer,
                        name,
                    };
                    commands.push(ok(to, HostResult::PairJoin(info)));
                }
                commands
            }
            PeersEvent::JoinFailed { id, error } => match self.joins.remove(&id) {
                Some(to) => vec![invalid(to, &error.to_string())],
                None => Vec::new(),
            },
            PeersEvent::Connected { peer } => {
                if self.trusted.contains_key(&peer) {
                    self.connected.insert(peer);
                }
                Vec::new()
            }
            PeersEvent::Disconnected { peer } => {
                self.connected.remove(&peer);
                Vec::new()
            }
            // The port already failed or refused it; nothing follows from it.
            PeersEvent::BindFailed { .. }
            | PeersEvent::ConnectFailed { .. }
            | PeersEvent::Refused { .. } => Vec::new(),
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
