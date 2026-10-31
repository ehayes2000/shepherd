//! The host's two links as one `Link` (T31a, decision 0019): its local socket, for this machine's clients, and its link
//! to other machines, whose conns come from trusted peers' hosts (and go to them, for tunnels). The runtime sees one
//! set of conns: each side's conn ids are mapped into one space (local ids even, peer ids odd), and each side's
//! events reach the runtime's sink through [`HostLinks::local_sink`] or [`HostLinks::peers_sink`], which map them
//! the same way.

use std::sync::Arc;
use std::time::{Duration, Instant};

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{ConnId, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink};

/// Which side a conn is on: the low bit of its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Local = 0,
    Peers = 1,
}

/// `conn` of `side`, in the shared space.
fn outer(side: Side, conn: ConnId) -> ConnId {
    ConnId::new(conn.get().wrapping_mul(2) | side as u64)
}

/// The side and id a shared id stands for.
fn inner(conn: ConnId) -> (Side, ConnId) {
    let side = match conn.get() & 1 {
        0 => Side::Local,
        _ => Side::Peers,
    };
    (side, ConnId::new(conn.get() >> 1))
}

/// Maps one side's events into the shared space.
struct SideSink {
    side: Side,
    sink: Arc<dyn LinkEventSink>,
}

impl LinkEventSink for SideSink {
    fn send(&self, event: LinkEvent) {
        let event = match event {
            LinkEvent::Connected { conn } => LinkEvent::Connected {
                conn: outer(self.side, conn),
            },
            LinkEvent::Line { conn, line } => LinkEvent::Line {
                conn: outer(self.side, conn),
                line,
            },
            LinkEvent::Disconnected { conn, reason } => LinkEvent::Disconnected {
                conn: outer(self.side, conn),
                reason,
            },
        };
        self.sink.send(event);
    }
}

/// The local link `L` and the link to other machines `P`, as one. Conns are opened on `P`.
pub struct HostLinks<L, P> {
    local: L,
    peers: P,
}

impl<L: Link, P: Link> HostLinks<L, P> {
    /// The sink to build the local link with, reporting to `sink`.
    pub fn local_sink(sink: Arc<dyn LinkEventSink>) -> Arc<dyn LinkEventSink> {
        Arc::new(SideSink {
            side: Side::Local,
            sink,
        })
    }

    /// The sink to build the link to other machines with, reporting to `sink`.
    pub fn peers_sink(sink: Arc<dyn LinkEventSink>) -> Arc<dyn LinkEventSink> {
        Arc::new(SideSink {
            side: Side::Peers,
            sink,
        })
    }

    /// `local` and `peers`, built with [`HostLinks::local_sink`] and [`HostLinks::peers_sink`].
    pub fn new(local: L, peers: P) -> Self {
        Self { local, peers }
    }

    pub fn local_mut(&mut self) -> &mut L {
        &mut self.local
    }

    pub fn peers_mut(&mut self) -> &mut P {
        &mut self.peers
    }

    fn on<T>(&self, conn: ConnId, f: impl FnOnce(&dyn Link, ConnId) -> T) -> T {
        match inner(conn) {
            (Side::Local, conn) => f(&self.local, conn),
            (Side::Peers, conn) => f(&self.peers, conn),
        }
    }

    fn on_mut<T>(&mut self, conn: ConnId, f: impl FnOnce(&mut dyn Link, ConnId) -> T) -> T {
        match inner(conn) {
            (Side::Local, conn) => f(&mut self.local, conn),
            (Side::Peers, conn) => f(&mut self.peers, conn),
        }
    }
}

impl<L: Link, P: Link> Link for HostLinks<L, P> {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        self.on_mut(conn, |link, conn| link.send(conn, line))
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.on_mut(conn, |link, conn| link.close(conn))
    }

    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.on(conn, |link, conn| link.queued_bytes(conn))
    }

    /// Both sides share the one deadline.
    fn flush_closed(&mut self, within: Duration) -> FlushOutcome {
        let deadline = Instant::now() + within;
        let local = self.local.flush_closed(within);
        let peers = self
            .peers
            .flush_closed(deadline.saturating_duration_since(Instant::now()));
        match (local, peers) {
            (FlushOutcome::Flushed, FlushOutcome::Flushed) => FlushOutcome::Flushed,
            _ => FlushOutcome::TimedOut,
        }
    }

    fn peer(&self, conn: ConnId) -> Option<MachineId> {
        self.on(conn, |link, conn| link.peer(conn))
    }

    fn drained_bytes(&self, conn: ConnId) -> Option<u64> {
        self.on(conn, |link, conn| link.drained_bytes(conn))
    }

    fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError> {
        self.on_mut(conn, |link, conn| link.pause_reading(conn, paused))
    }

    fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError> {
        self.peers.open(peer).map(|conn| outer(Side::Peers, conn))
    }
}

#[cfg(test)]
mod tests {
    use shepherd_testkit::contracts::link::{Events, LinkFixture};
    use shepherd_testkit::fakes::link::{FakeLink, FakeMachines, FakeNetwork};

    use super::*;

    /// Host links whose peers side is the host of a machine: the contract runs over that side (clients of other
    /// machines), with a local link beside it that nobody uses.
    struct OverPeers {
        machines: FakeMachines,
        local: FakeNetwork,
    }

    impl LinkFixture for OverPeers {
        type Host = HostLinks<FakeLink, FakeLink>;
        type Client = FakeLink;
        type Stalled = <FakeMachines as LinkFixture>::Stalled;

        fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> Self::Host {
            let local = self
                .local
                .listen(HostLinks::<FakeLink, FakeLink>::local_sink(sink.clone()))
                .unwrap_or_else(|e| panic!("{e}"));
            let peers = self
                .machines
                .listen(HostLinks::<FakeLink, FakeLink>::peers_sink(sink));
            HostLinks::new(local, peers)
        }

        fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (FakeLink, ConnId) {
            self.machines.connect(sink)
        }

        fn stalled_peer(&mut self) -> Self::Stalled {
            self.machines.stalled_peer()
        }

        fn machine_of(&self, client: &FakeLink) -> Option<MachineId> {
            self.machines.machine_of(client)
        }

        fn host_machine(&self) -> Option<MachineId> {
            self.machines.host_machine()
        }

        fn untrusted(&mut self, sink: Arc<dyn LinkEventSink>) -> Option<(FakeLink, ConnId)> {
            self.machines.untrusted(sink)
        }
    }

    /// Host links whose local side is a plain local link: the contract runs over that side.
    struct OverLocal {
        local: FakeNetwork,
        machines: FakeMachines,
    }

    impl LinkFixture for OverLocal {
        type Host = HostLinks<FakeLink, FakeLink>;
        type Client = FakeLink;
        type Stalled = <FakeNetwork as LinkFixture>::Stalled;

        fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> Self::Host {
            let local = self
                .local
                .listen(HostLinks::<FakeLink, FakeLink>::local_sink(sink.clone()))
                .unwrap_or_else(|e| panic!("{e}"));
            let peers = self
                .machines
                .listen(HostLinks::<FakeLink, FakeLink>::peers_sink(sink));
            HostLinks::new(local, peers)
        }

        fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (FakeLink, ConnId) {
            self.local.connect(sink).unwrap_or_else(|e| panic!("{e}"))
        }

        fn stalled_peer(&mut self) -> Self::Stalled {
            self.local.stalled_peer().unwrap_or_else(|e| panic!("{e}"))
        }

        fn machine_of(&self, _: &FakeLink) -> Option<MachineId> {
            None
        }

        fn host_machine(&self) -> Option<MachineId> {
            None
        }

        fn untrusted(&mut self, _: Arc<dyn LinkEventSink>) -> Option<(FakeLink, ConnId)> {
            None
        }
    }

    mod over_peers {
        use super::*;

        shepherd_testkit::link_contract!(OverPeers {
            machines: FakeMachines::new(),
            local: FakeNetwork::new(),
        });
    }

    mod over_local {
        use super::*;

        shepherd_testkit::link_contract!(OverLocal {
            local: FakeNetwork::new(),
            machines: FakeMachines::new(),
        });
    }

    #[test]
    fn conns_on_either_side_are_told_apart_and_conns_are_opened_on_the_peers_side() {
        let (sink, events) = Events::channel();
        let local_net = FakeNetwork::new();
        let world = FakeNetwork::new();
        let host = MachineId::new("host");
        let other = MachineId::new("other");
        let local = local_net
            .listen(HostLinks::<FakeLink, FakeLink>::local_sink(sink.clone()))
            .unwrap_or_else(|e| panic!("{e}"));
        let peers = world
            .listen_as(
                host.clone(),
                HostLinks::<FakeLink, FakeLink>::peers_sink(sink),
            )
            .unwrap_or_else(|e| panic!("{e}"));
        let mut links = HostLinks::new(local, peers);
        let (other_sink, other_events) = Events::channel();
        let _other = world
            .listen_as(other.clone(), other_sink)
            .unwrap_or_else(|e| panic!("{e}"));
        world.trust(&other, &host);
        let (client_sink, _client_events) = Events::channel();
        let (_client, _) = local_net
            .connect(client_sink)
            .unwrap_or_else(|e| panic!("{e}"));

        let opened = links.open(&other).unwrap_or_else(|e| panic!("{e}"));
        let seen = events.wait_for(
            |e| matches!(e, LinkEvent::Connected { conn } if *conn == opened),
            Duration::from_secs(1),
        );
        let LinkEvent::Connected { conn: local_conn } = seen[0] else {
            panic!("{seen:?}")
        };
        assert_ne!(local_conn, opened);
        assert_eq!(links.peer(opened), Some(other.clone()));
        assert_eq!(links.peer(local_conn), None);
        links.send(opened, b"hi").unwrap_or_else(|e| panic!("{e}"));
        let seen = other_events.wait_for(
            |e| matches!(e, LinkEvent::Line { .. }),
            Duration::from_secs(1),
        );
        assert!(matches!(seen.last(), Some(LinkEvent::Line { line, .. }) if line == b"hi"));
    }
}
