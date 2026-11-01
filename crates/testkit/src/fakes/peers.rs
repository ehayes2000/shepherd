//! An in-memory `Peers`. A [`FakePeerNet`] stands in for the network: each [`FakePeers`] on it is one machine, which
//! can reach another only while that one is bound and only at an address it learned (a pairing, a connection, or
//! [`FakePeerNet::introduce`]), as the real adapter can with address lookup off. A ticket is JSON naming the
//! inviter, its token and its name.
//!
//! Events are sent synchronously, from inside the call that caused them, while the network's lock is held, so sinks
//! must not call back into a port. [`FakePeers::fail_next_bind`] makes the next bind fail.
//!
//! Paths (T32b): two machines that connect report the path between them right after [`PeersEvent::Connected`]:
//! [`DEFAULT_PATH`] unless the test set another with [`FakePeerNet::set_path`], which also stands in for the path
//! changing (relayed to direct once hole punching works, or a new round-trip time) while they are connected.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::json;
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    ConnectError, JoinError, LinkKind, PairTicket, PairToken, PairingError, PairingId, PeerPath,
    Peers, PeersEvent, PeersEventSink, TOKEN_LEN,
};

use crate::contracts::peers::{Machine, PeersFixture, channel};

struct Node {
    sink: Arc<dyn PeersEventSink>,
    bound: bool,
    trusted: BTreeSet<MachineId>,
    pending: Option<(PairingId, PairToken, String)>,
    /// Machines whose address this one learned.
    known: BTreeSet<MachineId>,
    /// Machines this one has a connection with.
    connected: BTreeSet<MachineId>,
    fail_bind: Option<String>,
}

impl Node {
    fn send(&self, event: PeersEvent) {
        self.sink.send(event);
    }
}

/// The path between two machines on the network until a test sets another: direct, as on one LAN.
pub const DEFAULT_PATH: PeerPath = PeerPath {
    kind: LinkKind::Direct,
    rtt: Duration::from_millis(1),
};

#[derive(Default)]
struct Net {
    nodes: BTreeMap<MachineId, Node>,
    /// The path between two machines, by the pair (lower id first), where a test set one.
    paths: BTreeMap<(MachineId, MachineId), PeerPath>,
}

impl Net {
    fn node(&mut self, id: &MachineId) -> &mut Node {
        self.nodes
            .get_mut(id)
            .unwrap_or_else(|| unreachable!("FakePeers: {id} is not on this network"))
    }

    fn reachable(&self, from: &MachineId, to: &MachineId) -> bool {
        self.nodes[from].known.contains(to) && self.nodes.get(to).is_some_and(|n| n.bound)
    }

    fn path(&self, a: &MachineId, b: &MachineId) -> PeerPath {
        self.paths.get(&ends(a, b)).copied().unwrap_or(DEFAULT_PATH)
    }

    /// Tells `to` the path its connection with `peer` takes now.
    fn tell_path(&mut self, to: &MachineId, peer: &MachineId) {
        let path = self.path(to, peer);
        self.node(to).send(PeersEvent::Path {
            peer: peer.clone(),
            path,
        });
    }

    /// Drops the connection between `a` and `b`, if any: both ends hear it.
    fn disconnect(&mut self, a: &MachineId, b: &MachineId) {
        if self.node(a).connected.remove(b) {
            self.node(a)
                .send(PeersEvent::Disconnected { peer: b.clone() });
        }
        if self.node(b).connected.remove(a) {
            self.node(b)
                .send(PeersEvent::Disconnected { peer: a.clone() });
        }
    }
}

/// The key for the pair `a`, `b`, whichever way round.
fn ends(a: &MachineId, b: &MachineId) -> (MachineId, MachineId) {
    match a <= b {
        true => (a.clone(), b.clone()),
        false => (b.clone(), a.clone()),
    }
}

/// The network the fake machines are on; also the contract's fixture.
#[derive(Clone, Default)]
pub struct FakePeerNet {
    net: Arc<Mutex<Net>>,
}

/// Every fresh machine is a different one.
static NEXT_MACHINE: AtomicU64 = AtomicU64::new(1);

impl FakePeerNet {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Net> {
        self.net.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A machine called `id` on this network, reporting to `sink`: not bound, trusting nobody. A machine that was on
    /// the network before (a host restarting) keeps the addresses it learned, as the real adapter's last-address cache
    /// keeps them; its old port's connections are gone.
    pub fn peers(&self, id: MachineId, sink: Arc<dyn PeersEventSink>) -> FakePeers {
        let mut net = self.lock();
        let known = match net.nodes.get(&id) {
            Some(old) => {
                let gone: Vec<MachineId> = old.connected.iter().cloned().collect();
                for peer in gone {
                    net.disconnect(&id, &peer);
                }
                net.node(&id).known.clone()
            }
            None => BTreeSet::new(),
        };
        net.nodes.insert(
            id.clone(),
            Node {
                sink,
                bound: false,
                trusted: BTreeSet::new(),
                pending: None,
                known,
                connected: BTreeSet::new(),
                fail_bind: None,
            },
        );
        drop(net);
        FakePeers {
            net: self.clone(),
            id,
        }
    }

    /// Tells machine `from` where machine `to` is.
    pub fn introduce(&self, from: &MachineId, to: &MachineId) {
        self.lock().node(from).known.insert(to.clone());
    }

    /// The machines `id` trusts now.
    pub fn trusted(&self, id: &MachineId) -> BTreeSet<MachineId> {
        self.lock().node(id).trusted.clone()
    }

    /// Whether machine `id` is bound now.
    pub fn is_bound(&self, id: &MachineId) -> bool {
        self.lock().node(id).bound
    }

    /// The path between machines `a` and `b` is `path` from now on. While they are connected, each end hears it at once
    /// (if it changed).
    pub fn set_path(&self, a: &MachineId, b: &MachineId, path: PeerPath) {
        let mut net = self.lock();
        let before = net.paths.insert(ends(a, b), path);
        if before.unwrap_or(DEFAULT_PATH) == path {
            return;
        }
        if net.nodes.get(a).is_some_and(|n| n.connected.contains(b)) {
            net.tell_path(a, b);
        }
        if net.nodes.get(b).is_some_and(|n| n.connected.contains(a)) {
            net.tell_path(b, a);
        }
    }

    /// Whether machine `id` has a pairing open now.
    pub fn is_pairing(&self, id: &MachineId) -> bool {
        self.lock().node(id).pending.is_some()
    }
}

impl PeersFixture for FakePeerNet {
    type Peers = FakePeers;

    fn machine(&mut self) -> Machine<FakePeers> {
        let n = NEXT_MACHINE.fetch_add(1, Ordering::Relaxed);
        let id = MachineId::new(format!("{n:064x}"));
        let (sink, events, paths) = channel();
        Machine {
            peers: self.peers(id.clone(), sink),
            id,
            events,
            paths,
        }
    }

    fn introduce(&mut self, from: &mut Machine<FakePeers>, to: &Machine<FakePeers>) {
        FakePeerNet::introduce(self, &from.id, &to.id);
    }
}

/// One machine on a [`FakePeerNet`].
pub struct FakePeers {
    net: FakePeerNet,
    id: MachineId,
}

impl FakePeers {
    /// The next bind fails with `error`, and leaves the machine unbound.
    pub fn fail_next_bind(&self, error: &str) {
        self.net.lock().node(&self.id).fail_bind = Some(error.to_owned());
    }

    fn ticket(&self, token: &PairToken, name: &str) -> PairTicket {
        let value =
            json!({ "fake-pair": { "id": self.id, "token": token.as_bytes(), "name": name } });
        PairTicket::new(value.to_string())
    }
}

/// What a fake ticket names: the inviter, its token and its name.
fn parse_ticket(ticket: &PairTicket) -> Option<(MachineId, PairToken)> {
    let value: serde_json::Value = serde_json::from_str(ticket.as_str()).ok()?;
    let inner = value.get("fake-pair")?;
    let id = MachineId::new(inner.get("id")?.as_str()?);
    let bytes: Vec<u8> = serde_json::from_value(inner.get("token")?.clone()).ok()?;
    let token = <[u8; TOKEN_LEN]>::try_from(bytes.as_slice()).ok()?;
    Some((id, PairToken::new(token)))
}

impl Peers for FakePeers {
    fn bind(&mut self) {
        let mut net = self.net.lock();
        let node = net.node(&self.id);
        if let Some(error) = node.fail_bind.take() {
            node.send(PeersEvent::BindFailed { error });
            return;
        }
        node.bound = true;
    }

    fn unbind(&mut self) {
        let mut net = self.net.lock();
        let node = net.node(&self.id);
        node.bound = false;
        node.pending = None;
        let connected: Vec<MachineId> = node.connected.iter().cloned().collect();
        for peer in connected {
            net.disconnect(&self.id, &peer);
        }
    }

    fn set_trusted(&mut self, peers: &BTreeSet<MachineId>) {
        let mut net = self.net.lock();
        let node = net.node(&self.id);
        node.trusted = peers.clone();
        let dropped: Vec<MachineId> = node.connected.difference(peers).cloned().collect();
        for peer in dropped {
            net.disconnect(&self.id, &peer);
        }
    }

    fn accept_pairing(&mut self, id: PairingId, token: PairToken, name: &str) {
        let ticket = self.ticket(&token, name);
        let mut net = self.net.lock();
        let node = net.node(&self.id);
        if !node.bound {
            node.send(PeersEvent::PairingFailed {
                id,
                error: PairingError::Unbound,
            });
            return;
        }
        node.pending = Some((id, token, name.to_owned()));
        node.send(PeersEvent::PairingOpen { id, ticket });
    }

    fn stop_pairing(&mut self) {
        self.net.lock().node(&self.id).pending = None;
    }

    fn join(&mut self, id: PairingId, ticket: &PairTicket, name: &str) {
        let mut net = self.net.lock();
        let fail = |net: &mut Net, error| {
            net.node(&self.id)
                .send(PeersEvent::JoinFailed { id, error });
        };
        let Some((inviter, token)) = parse_ticket(ticket) else {
            return fail(&mut net, JoinError::BadTicket);
        };
        if !net.node(&self.id).bound {
            return fail(&mut net, JoinError::Unbound);
        }
        // The ticket says where the inviter is.
        net.node(&self.id).known.insert(inviter.clone());
        if !net.reachable(&self.id, &inviter) {
            return fail(&mut net, JoinError::Unreachable("not bound".into()));
        }
        let host = net.node(&inviter);
        let Some((pairing, expected, host_name)) = host.pending.take() else {
            return fail(&mut net, JoinError::NotOpen);
        };
        let accepted = expected == token;
        host.send(PeersEvent::PairRequested {
            id: pairing,
            peer: self.id.clone(),
            name: name.to_owned(),
            accepted,
        });
        if !accepted {
            return fail(&mut net, JoinError::Refused);
        }
        // The inviter learned the joiner's address from the attempt.
        host.known.insert(self.id.clone());
        net.node(&self.id).send(PeersEvent::Paired {
            id,
            peer: inviter,
            name: host_name,
        });
    }

    fn connect(&mut self, peer: &MachineId) {
        let mut net = self.net.lock();
        let fail = |net: &mut Net, error| {
            net.node(&self.id).send(PeersEvent::ConnectFailed {
                peer: peer.clone(),
                error,
            });
        };
        if !net.node(&self.id).bound {
            return fail(&mut net, ConnectError::Unbound);
        }
        if !net.node(&self.id).trusted.contains(peer) {
            return fail(&mut net, ConnectError::Untrusted);
        }
        if !net.reachable(&self.id, peer) {
            return fail(
                &mut net,
                ConnectError::Unreachable("no known address".into()),
            );
        }
        let remote = net.node(peer);
        if !remote.trusted.contains(&self.id) {
            remote.send(PeersEvent::Refused {
                peer: self.id.clone(),
            });
            return fail(&mut net, ConnectError::Refused);
        }
        remote.known.insert(self.id.clone());
        if remote.connected.insert(self.id.clone()) {
            remote.send(PeersEvent::Connected {
                peer: self.id.clone(),
            });
            net.tell_path(peer, &self.id);
        }
        let local = net.node(&self.id);
        if local.connected.insert(peer.clone()) {
            local.send(PeersEvent::Connected { peer: peer.clone() });
            net.tell_path(&self.id, peer);
        }
    }
}

#[cfg(test)]
mod tests {
    mod contract {
        use super::super::FakePeerNet;

        crate::peers_contract!(FakePeerNet::new());
    }

    use std::collections::BTreeSet;

    use shepherd_core::ports::{Peers, PeersEvent};

    use super::*;
    use crate::contracts::peers::{Machine, PeersFixture};

    /// Two bound machines, each trusting the other, connected; their opening paths read.
    fn connected(net: &mut FakePeerNet) -> (Machine<FakePeers>, Machine<FakePeers>) {
        let mut a = net.machine();
        let mut b = net.machine();
        a.peers.bind();
        b.peers.bind();
        a.peers.set_trusted(&BTreeSet::from([b.id.clone()]));
        b.peers.set_trusted(&BTreeSet::from([a.id.clone()]));
        FakePeerNet::introduce(net, &b.id, &a.id);
        b.peers.connect(&a.id);
        assert!(matches!(b.next(), PeersEvent::Connected { .. }));
        assert!(matches!(a.next(), PeersEvent::Connected { .. }));
        assert_eq!(b.next_path(), (a.id.clone(), DEFAULT_PATH));
        assert_eq!(a.next_path(), (b.id.clone(), DEFAULT_PATH));
        (a, b)
    }

    #[test]
    fn a_path_that_changes_while_connected_is_reported_at_both_ends() {
        let mut net = FakePeerNet::new();
        let (a, b) = connected(&mut net);
        let relayed = PeerPath {
            kind: LinkKind::Relayed,
            rtt: Duration::from_millis(86),
        };
        net.set_path(&a.id, &b.id, relayed);
        assert_eq!(a.next_path(), (b.id.clone(), relayed));
        assert_eq!(b.next_path(), (a.id.clone(), relayed));
        // Hole punching worked.
        let direct = PeerPath {
            kind: LinkKind::Direct,
            rtt: Duration::from_millis(4),
        };
        net.set_path(&b.id, &a.id, direct);
        assert_eq!(a.next_path(), (b.id.clone(), direct));
        assert_eq!(b.next_path(), (a.id.clone(), direct));
        // The same path again is no change.
        net.set_path(&a.id, &b.id, direct);
        assert!(a.paths.try_recv().is_err() && b.paths.try_recv().is_err());
    }

    #[test]
    fn a_path_set_before_connecting_is_the_one_reported_on_connecting() {
        let mut net = FakePeerNet::new();
        let mut a = net.machine();
        let mut b = net.machine();
        let relayed = PeerPath {
            kind: LinkKind::Relayed,
            rtt: Duration::from_millis(86),
        };
        net.set_path(&a.id, &b.id, relayed);
        assert!(
            a.paths.try_recv().is_err(),
            "not connected: nothing to report"
        );
        a.peers.bind();
        b.peers.bind();
        a.peers.set_trusted(&BTreeSet::from([b.id.clone()]));
        b.peers.set_trusted(&BTreeSet::from([a.id.clone()]));
        FakePeerNet::introduce(&net, &b.id, &a.id);
        b.peers.connect(&a.id);
        assert_eq!(b.next_path(), (a.id.clone(), relayed));
        assert_eq!(a.next_path(), (b.id.clone(), relayed));
    }

    #[test]
    fn a_bind_set_to_fail_fails_once_and_leaves_the_machine_unbound() {
        let mut net = FakePeerNet::new();
        let mut a = net.machine();
        a.peers.fail_next_bind("no interfaces");
        a.peers.bind();
        assert_eq!(
            a.next(),
            PeersEvent::BindFailed {
                error: "no interfaces".into()
            }
        );
        assert!(!net.is_bound(&a.id));
        a.peers.bind();
        assert!(net.is_bound(&a.id));
        a.peers.set_trusted(&BTreeSet::from([a.id.clone()]));
        assert_eq!(net.trusted(&a.id), BTreeSet::from([a.id.clone()]));
    }
}
