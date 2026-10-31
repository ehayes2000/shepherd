//! The `Peers` contract. Every case needs two or three machines that can reach each other, so they are built through
//! a [`PeersFixture`]: the fake's fixture puts them on one in-memory network, the iroh adapter's binds real endpoints on
//! localhost with relays and address lookup off (no network). A machine reaches another only at an address it learned:
//! from a pairing (either side), from an earlier connection, or from [`PeersFixture::introduce`], which stands in for
//! looking the id up.
//!
//! Run it with [`peers_contract!`](crate::peers_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Expands to one `#[test]` per `Peers` contract case. `$fixture` is an expression building a fresh [`PeersFixture`]
/// for each test, e.g. `shepherd_testkit::peers_contract!(FakePeerNet::new());`.
///
/// New cases go in the list below.
#[macro_export]
macro_rules! peers_contract {
    ($fixture:expr) => {
        $crate::peers_contract!(@cases $fixture;
            a_ticket_pairs_two_machines_each_learning_the_others_name,
            a_wrong_token_is_refused_and_uses_the_pairing_up,
            a_ticket_works_once,
            a_stopped_pairing_refuses_its_ticket,
            a_token_differing_in_its_last_byte_is_refused,
            text_that_is_not_a_ticket_is_a_bad_ticket,
            an_unbound_machine_cannot_open_join_or_connect,
            binding_twice_is_harmless,
            a_trusted_machine_connects_and_both_ends_see_it,
            an_untrusted_machine_is_refused_before_any_connected,
            a_machine_trusted_after_a_refusal_can_connect,
            paired_machines_reach_each_other_without_an_introduction,
            untrusting_a_machine_drops_its_connection_at_once,
            unbinding_drops_every_connection,
            dialling_a_machine_nobody_knows_fails,
            dialling_a_machine_this_one_does_not_trust_fails_without_a_word_to_it,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::peers::$case(&mut $fixture);
            }
        )*
    };
}

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    ConnectError, JoinError, PairTicket, PairToken, PairingError, PairingId, Peers, PeersEvent,
    PeersEventSink,
};

/// How long a case waits for an event: a liveness bound for real endpoints on localhost, never a measurement.
pub const EVENT_LIMIT: Duration = Duration::from_secs(20);

/// A sink that forwards events to an mpsc channel. Send errors (receiver gone) are ignored.
pub struct ChannelSink(pub Sender<PeersEvent>);

impl PeersEventSink for ChannelSink {
    fn send(&self, event: PeersEvent) {
        let _ = self.0.send(event);
    }
}

/// A new sink and the receiving end of its events.
pub fn channel() -> (Arc<dyn PeersEventSink>, Receiver<PeersEvent>) {
    let (tx, rx) = mpsc::channel();
    (Arc::new(ChannelSink(tx)), rx)
}

/// One machine: its port, its id, and what its port reports.
pub struct Machine<P> {
    pub peers: P,
    pub id: MachineId,
    pub events: Receiver<PeersEvent>,
}

impl<P> Machine<P> {
    /// The next event, waiting up to [`EVENT_LIMIT`].
    pub fn next(&self) -> PeersEvent {
        self.events
            .recv_timeout(EVENT_LIMIT)
            .unwrap_or_else(|e| panic!("{}: no event within {EVENT_LIMIT:?}: {e}", self.id))
    }

    /// Every event already reported and not read yet.
    pub fn pending(&self) -> Vec<PeersEvent> {
        self.events.try_iter().collect()
    }
}

/// Builds machines that can reach each other.
pub trait PeersFixture {
    type Peers: Peers;
    /// A new machine with a key of its own, not bound, trusting nobody.
    fn machine(&mut self) -> Machine<Self::Peers>;
    /// Tells `from` where `to` is, as looking `to`'s id up would. `to` must be bound.
    fn introduce(&mut self, from: &mut Machine<Self::Peers>, to: &Machine<Self::Peers>);
}

fn token(byte: u8) -> PairToken {
    PairToken::new([byte; 16])
}

fn trusting<P: Peers>(machine: &mut Machine<P>, others: &[&MachineId]) {
    let set: BTreeSet<MachineId> = others.iter().map(|id| (*id).clone()).collect();
    machine.peers.set_trusted(&set);
}

/// A bound machine.
fn bound<F: PeersFixture>(f: &mut F) -> Machine<F::Peers> {
    let mut machine = f.machine();
    machine.peers.bind();
    machine
}

/// Opens pairing `id` on `inviter` with `token`, and returns its ticket.
fn open<P: Peers>(inviter: &mut Machine<P>, id: u64, token: PairToken) -> PairTicket {
    inviter
        .peers
        .accept_pairing(PairingId::new(id), token, "inviter");
    match inviter.next() {
        PeersEvent::PairingOpen { id: open, ticket } if open == PairingId::new(id) => ticket,
        other => panic!("expected pairing {id} open, got {other:?}"),
    }
}

/// `joiner` joins with `ticket` as join `id`, and returns how that ended for it.
fn join<P: Peers>(joiner: &mut Machine<P>, id: u64, ticket: &PairTicket) -> PeersEvent {
    joiner.peers.join(PairingId::new(id), ticket, "joiner");
    joiner.next()
}

/// Pairs two bound machines; each then trusts the other.
fn pair<P: Peers>(inviter: &mut Machine<P>, joiner: &mut Machine<P>) {
    let ticket = open(inviter, 1, token(7));
    assert!(matches!(
        join(joiner, 1, &ticket),
        PeersEvent::Paired { .. }
    ));
    assert!(matches!(
        inviter.next(),
        PeersEvent::PairRequested { accepted: true, .. }
    ));
    trusting(inviter, &[&joiner.id]);
    trusting(joiner, &[&inviter.id]);
}

fn refused_join() -> PeersEvent {
    PeersEvent::JoinFailed {
        id: PairingId::new(1),
        error: JoinError::Refused,
    }
}

pub fn a_ticket_pairs_two_machines_each_learning_the_others_name<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    let ticket = open(&mut a, 3, token(7));
    assert_eq!(
        join(&mut b, 9, &ticket),
        PeersEvent::Paired {
            id: PairingId::new(9),
            peer: a.id.clone(),
            name: "inviter".into(),
        }
    );
    assert_eq!(
        a.next(),
        PeersEvent::PairRequested {
            id: PairingId::new(3),
            peer: b.id.clone(),
            name: "joiner".into(),
            accepted: true,
        }
    );
}

pub fn a_wrong_token_is_refused_and_uses_the_pairing_up<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    let mut c = bound(f);
    let old = open(&mut a, 1, token(1));
    // A newer pairing replaces the first: the old ticket's token is now the wrong one.
    let new = open(&mut a, 2, token(2));
    assert_eq!(join(&mut b, 1, &old), refused_join());
    assert_eq!(
        a.next(),
        PeersEvent::PairRequested {
            id: PairingId::new(2),
            peer: b.id.clone(),
            name: "joiner".into(),
            accepted: false,
        }
    );
    // Used up: even the right ticket is refused now, and nobody is asked about it.
    assert_eq!(join(&mut c, 1, &new), refused_join());
    assert_eq!(a.pending(), vec![]);
}

pub fn a_ticket_works_once<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    let mut c = bound(f);
    let ticket = open(&mut a, 1, token(7));
    assert!(matches!(
        join(&mut b, 1, &ticket),
        PeersEvent::Paired { .. }
    ));
    assert!(matches!(
        a.next(),
        PeersEvent::PairRequested { accepted: true, .. }
    ));
    assert_eq!(join(&mut c, 1, &ticket), refused_join());
    assert_eq!(a.pending(), vec![]);
}

pub fn a_stopped_pairing_refuses_its_ticket<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    let ticket = open(&mut a, 1, token(7));
    a.peers.stop_pairing();
    assert_eq!(join(&mut b, 1, &ticket), refused_join());
    assert_eq!(a.pending(), vec![]);
}

pub fn a_token_differing_in_its_last_byte_is_refused<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    let mut almost = [7; 16];
    almost[15] = 8;
    let old = open(&mut a, 1, PairToken::new(almost));
    open(&mut a, 2, token(7));
    assert_eq!(join(&mut b, 1, &old), refused_join());
    assert!(matches!(
        a.next(),
        PeersEvent::PairRequested {
            accepted: false,
            ..
        }
    ));
}

pub fn text_that_is_not_a_ticket_is_a_bad_ticket<F: PeersFixture>(f: &mut F) {
    let mut b = bound(f);
    assert_eq!(
        join(&mut b, 4, &PairTicket::new("not a ticket")),
        PeersEvent::JoinFailed {
            id: PairingId::new(4),
            error: JoinError::BadTicket,
        }
    );
}

pub fn an_unbound_machine_cannot_open_join_or_connect<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let ticket = open(&mut a, 1, token(7));
    let mut b = f.machine();
    b.peers.accept_pairing(PairingId::new(5), token(1), "b");
    assert_eq!(
        b.next(),
        PeersEvent::PairingFailed {
            id: PairingId::new(5),
            error: PairingError::Unbound,
        }
    );
    assert_eq!(
        join(&mut b, 6, &ticket),
        PeersEvent::JoinFailed {
            id: PairingId::new(6),
            error: JoinError::Unbound,
        }
    );
    b.peers.connect(&a.id);
    assert_eq!(
        b.next(),
        PeersEvent::ConnectFailed {
            peer: a.id.clone(),
            error: ConnectError::Unbound,
        }
    );
    // Unbinding after binding is the same.
    a.peers.unbind();
    a.peers.accept_pairing(PairingId::new(2), token(1), "a");
    assert!(matches!(a.next(), PeersEvent::PairingFailed { .. }));
}

pub fn binding_twice_is_harmless<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    a.peers.bind();
    let mut b = bound(f);
    let ticket = open(&mut a, 1, token(7));
    assert!(matches!(
        join(&mut b, 1, &ticket),
        PeersEvent::Paired { .. }
    ));
}

pub fn a_trusted_machine_connects_and_both_ends_see_it<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    trusting(&mut a, &[&b.id]);
    trusting(&mut b, &[&a.id]);
    f.introduce(&mut b, &a);
    b.peers.connect(&a.id);
    assert_eq!(b.next(), PeersEvent::Connected { peer: a.id.clone() });
    assert_eq!(a.next(), PeersEvent::Connected { peer: b.id.clone() });
}

pub fn an_untrusted_machine_is_refused_before_any_connected<F: PeersFixture>(f: &mut F) {
    let a = bound(f);
    let mut c = bound(f);
    trusting(&mut c, &[&a.id]);
    f.introduce(&mut c, &a);
    c.peers.connect(&a.id);
    assert_eq!(
        c.next(),
        PeersEvent::ConnectFailed {
            peer: a.id.clone(),
            error: ConnectError::Refused,
        }
    );
    // The refusal is all `a` ever said about `c`.
    assert_eq!(a.next(), PeersEvent::Refused { peer: c.id.clone() });
    assert_eq!(a.pending(), vec![]);
}

pub fn a_machine_trusted_after_a_refusal_can_connect<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut c = bound(f);
    trusting(&mut c, &[&a.id]);
    f.introduce(&mut c, &a);
    c.peers.connect(&a.id);
    assert!(matches!(c.next(), PeersEvent::ConnectFailed { .. }));
    assert!(matches!(a.next(), PeersEvent::Refused { .. }));
    trusting(&mut a, &[&c.id]);
    c.peers.connect(&a.id);
    assert_eq!(c.next(), PeersEvent::Connected { peer: a.id.clone() });
    assert_eq!(a.next(), PeersEvent::Connected { peer: c.id.clone() });
}

pub fn paired_machines_reach_each_other_without_an_introduction<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    pair(&mut a, &mut b);
    // The joiner learned the inviter's address from the ticket...
    b.peers.connect(&a.id);
    assert_eq!(b.next(), PeersEvent::Connected { peer: a.id.clone() });
    assert_eq!(a.next(), PeersEvent::Connected { peer: b.id.clone() });
    // ...and the inviter learned the joiner's from the pairing.
    let mut c = bound(f);
    let mut d = bound(f);
    pair(&mut c, &mut d);
    c.peers.connect(&d.id);
    assert_eq!(c.next(), PeersEvent::Connected { peer: d.id.clone() });
    assert_eq!(d.next(), PeersEvent::Connected { peer: c.id.clone() });
}

pub fn untrusting_a_machine_drops_its_connection_at_once<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    pair(&mut a, &mut b);
    b.peers.connect(&a.id);
    assert!(matches!(b.next(), PeersEvent::Connected { .. }));
    assert!(matches!(a.next(), PeersEvent::Connected { .. }));
    trusting(&mut a, &[]);
    assert_eq!(a.next(), PeersEvent::Disconnected { peer: b.id.clone() });
    assert_eq!(b.next(), PeersEvent::Disconnected { peer: a.id.clone() });
    // And it can't come back.
    b.peers.connect(&a.id);
    assert_eq!(
        b.next(),
        PeersEvent::ConnectFailed {
            peer: a.id.clone(),
            error: ConnectError::Refused,
        }
    );
    assert_eq!(a.next(), PeersEvent::Refused { peer: b.id.clone() });
}

pub fn unbinding_drops_every_connection<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let mut b = bound(f);
    pair(&mut a, &mut b);
    b.peers.connect(&a.id);
    assert!(matches!(b.next(), PeersEvent::Connected { .. }));
    assert!(matches!(a.next(), PeersEvent::Connected { .. }));
    b.peers.unbind();
    assert_eq!(b.next(), PeersEvent::Disconnected { peer: a.id.clone() });
    assert_eq!(a.next(), PeersEvent::Disconnected { peer: b.id.clone() });
}

pub fn dialling_a_machine_nobody_knows_fails<F: PeersFixture>(f: &mut F) {
    let mut a = bound(f);
    let stranger = f.machine();
    trusting(&mut a, &[&stranger.id]);
    a.peers.connect(&stranger.id);
    assert!(
        matches!(
            a.next(),
            PeersEvent::ConnectFailed { ref peer, error: ConnectError::Unreachable(_) } if *peer == stranger.id
        ),
        "an unbound machine nobody introduced can't be reached"
    );
}

pub fn dialling_a_machine_this_one_does_not_trust_fails_without_a_word_to_it<F: PeersFixture>(
    f: &mut F,
) {
    let mut a = bound(f);
    let mut c = bound(f);
    trusting(&mut a, &[&c.id]);
    f.introduce(&mut c, &a);
    c.peers.connect(&a.id);
    assert_eq!(
        c.next(),
        PeersEvent::ConnectFailed {
            peer: a.id.clone(),
            error: ConnectError::Untrusted,
        }
    );
    assert_eq!(a.pending(), vec![]);
}
