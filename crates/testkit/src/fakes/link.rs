//! An in-memory `Link`. A [`FakeNetwork`] stands in for the transport: one host listens on it, and clients
//! connect through it. Each conn is a pair of ends, one per link, and every end has its own [`ConnId`].
//!
//! Events are sent synchronously, from inside the call that caused them, while the network's lock is held, so
//! sinks must not call back into a link. A [`StalledPeer`] never reads: lines sent to it pile up in its queue
//! until [`MAX_QUEUED`] is passed, which disconnects it as a slow consumer, just like a socket peer that stops
//! reading. A paced client ([`FakeNetwork::connect_paced`]) reads only when the test says so
//! ([`FakeNetwork::deliver`]), so a test can play a slow reader and watch [`Link::queued_bytes`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd_core::ports::{
    ConnId, DisconnectReason, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED,
};
use shepherd_core::wire::MAX_LINE;

use crate::contracts::link::LinkFixture;

/// Which link an end belongs to.
type LinkId = u64;

struct End {
    owner: LinkId,
    /// `None` for a stalled peer, which never reads.
    sink: Option<Arc<dyn LinkEventSink>>,
    /// Holds lines until [`FakeNetwork::deliver`] reads them, instead of delivering them at once.
    paced: bool,
    peer: ConnId,
    /// Lines sent to a paced end and not yet delivered.
    pending: VecDeque<Vec<u8>>,
    /// Bytes sent to this end and not yet read. Only a stalled or paced peer leaves bytes unread.
    unread: usize,
}

impl End {
    fn new(owner: LinkId, sink: Option<Arc<dyn LinkEventSink>>, peer: ConnId) -> Self {
        Self {
            owner,
            sink,
            paced: false,
            peer,
            pending: VecDeque::new(),
            unread: 0,
        }
    }
}

#[derive(Default)]
struct Net {
    /// The listening host's link and sink.
    host: Option<(LinkId, Arc<dyn LinkEventSink>)>,
    ends: HashMap<ConnId, End>,
    next_id: u64,
}

impl Net {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Opens a conn from a new client link to the host. Returns the client link and its end.
    fn connect(
        &mut self,
        sink: Option<Arc<dyn LinkEventSink>>,
        paced: bool,
    ) -> Result<(LinkId, ConnId), LinkError> {
        let (host, host_sink) = self
            .host
            .clone()
            .ok_or_else(|| LinkError::Io("connection refused".into()))?;
        let link = self.next_id();
        let host_end = ConnId::new(self.next_id());
        let client_end = ConnId::new(self.next_id());
        self.ends.insert(
            host_end,
            End::new(host, Some(host_sink.clone()), client_end),
        );
        let mut client = End::new(link, sink, host_end);
        client.paced = paced;
        self.ends.insert(client_end, client);
        host_sink.send(LinkEvent::Connected { conn: host_end });
        Ok((link, client_end))
    }

    fn end(&mut self, link: LinkId, conn: ConnId) -> Result<&mut End, LinkError> {
        self.ends
            .get_mut(&conn)
            .filter(|end| end.owner == link)
            .ok_or(LinkError::UnknownConn)
    }

    fn send(&mut self, link: LinkId, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        let peer = self.end(link, conn)?.peer;
        if line.contains(&b'\n') {
            return Err(LinkError::InvalidLine);
        }
        if line.len() > MAX_LINE {
            let reason = DisconnectReason::Protocol(format!("line exceeds {MAX_LINE} bytes"));
            self.disconnect(peer, reason, DisconnectReason::PeerGone);
            return Ok(());
        }
        let Some(to) = self.ends.get_mut(&peer) else {
            return Ok(());
        };
        match &to.sink {
            Some(sink) if !to.paced => sink.send(LinkEvent::Line {
                conn: peer,
                line: line.to_vec(),
            }),
            _ if to.unread > 0 && to.unread + line.len() > MAX_QUEUED => {
                self.disconnect(
                    conn,
                    DisconnectReason::SlowConsumer,
                    DisconnectReason::PeerGone,
                );
            }
            _ => {
                to.unread += line.len();
                if to.paced {
                    to.pending.push_back(line.to_vec());
                }
            }
        }
        Ok(())
    }

    /// What `conn`'s peer hasn't read yet.
    fn queued_bytes(&self, link: LinkId, conn: ConnId) -> Option<usize> {
        let end = self.ends.get(&conn).filter(|end| end.owner == link)?;
        Some(self.ends.get(&end.peer).map_or(0, |peer| peer.unread))
    }

    /// Delivers lines pending for the paced end `conn`, oldest first, up to `max_bytes` (always at least one
    /// line if any is pending). Returns the bytes delivered.
    fn deliver(&mut self, conn: ConnId, max_bytes: usize) -> usize {
        let Some(end) = self.ends.get_mut(&conn) else {
            return 0;
        };
        let mut delivered = 0;
        while let Some(line) = end.pending.front() {
            if delivered > 0 && delivered + line.len() > max_bytes {
                break;
            }
            let line = end.pending.pop_front().unwrap_or_default();
            delivered += line.len();
            end.unread -= line.len();
            if let Some(sink) = &end.sink {
                sink.send(LinkEvent::Line { conn, line });
            }
        }
        delivered
    }

    fn close(&mut self, link: LinkId, conn: ConnId) -> Result<(), LinkError> {
        self.end(link, conn)?;
        self.disconnect(conn, DisconnectReason::Closed, DisconnectReason::PeerGone);
        Ok(())
    }

    /// Removes the conn `end` belongs to. `end` gets `here`, its peer gets `there`.
    fn disconnect(&mut self, end: ConnId, here: DisconnectReason, there: DisconnectReason) {
        let Some(this) = self.ends.remove(&end) else {
            return;
        };
        let peer = self.ends.remove(&this.peer);
        if let Some(sink) = this.sink {
            sink.send(LinkEvent::Disconnected {
                conn: end,
                reason: here,
            });
        }
        if let Some(sink) = peer.and_then(|p| p.sink) {
            sink.send(LinkEvent::Disconnected {
                conn: this.peer,
                reason: there,
            });
        }
    }

    /// Disconnects every conn of `link` without flushing, as when its process dies.
    fn drop_link(&mut self, link: LinkId) {
        if self.host.as_ref().is_some_and(|(host, _)| *host == link) {
            self.host = None;
        }
        let mut owned: Vec<ConnId> = self
            .ends
            .iter()
            .filter(|(_, end)| end.owner == link)
            .map(|(&conn, _)| conn)
            .collect();
        owned.sort();
        for conn in owned {
            self.disconnect(conn, DisconnectReason::Closed, DisconnectReason::PeerGone);
        }
    }
}

/// The transport the fake links share. Cheap to clone; clones are the same network.
#[derive(Clone, Default)]
pub struct FakeNetwork(Arc<Mutex<Net>>);

impl FakeNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the host end. Fails if a host is already listening.
    pub fn listen(&self, sink: Arc<dyn LinkEventSink>) -> Result<FakeLink, LinkError> {
        let mut net = self.lock();
        if net.host.is_some() {
            return Err(LinkError::Io("address in use".into()));
        }
        let id = net.next_id();
        net.host = Some((id, sink));
        Ok(FakeLink::new(self.clone(), id))
    }

    /// Connects a client end to the listening host. Fails if no host is listening.
    pub fn connect(&self, sink: Arc<dyn LinkEventSink>) -> Result<(FakeLink, ConnId), LinkError> {
        let (id, conn) = self.lock().connect(Some(sink), false)?;
        Ok((FakeLink::new(self.clone(), id), conn))
    }

    /// Connects a client that reads only when [`FakeNetwork::deliver`] is called: a slow reader. Lines sent to it
    /// count towards the sender's [`Link::queued_bytes`] and its [`MAX_QUEUED`] bound until delivered.
    pub fn connect_paced(
        &self,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<(FakeLink, ConnId), LinkError> {
        let (id, conn) = self.lock().connect(Some(sink), true)?;
        Ok((FakeLink::new(self.clone(), id), conn))
    }

    /// Lets the paced client end `conn` read up to `max_bytes` of what was sent to it (at least one line, if any
    /// is waiting). Returns the bytes read.
    pub fn deliver(&self, conn: ConnId, max_bytes: usize) -> usize {
        self.lock().deliver(conn, max_bytes)
    }

    /// Connects a peer that never reads. Dropping it is an abrupt disconnect.
    pub fn stalled_peer(&self) -> Result<StalledPeer, LinkError> {
        let (id, _) = self.lock().connect(None, false)?;
        Ok(StalledPeer {
            net: self.clone(),
            id,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Net> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One end of the fake transport: the host's (many conns) or a client's (one conn).
pub struct FakeLink {
    net: FakeNetwork,
    id: LinkId,
    fail_next: Option<LinkError>,
}

impl FakeLink {
    fn new(net: FakeNetwork, id: LinkId) -> Self {
        Self {
            net,
            id,
            fail_next: None,
        }
    }

    /// Makes the next `send` or `close` on this link fail with `error`, doing nothing else.
    pub fn fail_next(&mut self, error: LinkError) {
        self.fail_next = Some(error);
    }
}

impl Link for FakeLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        if let Some(error) = self.fail_next.take() {
            return Err(error);
        }
        self.net.lock().send(self.id, conn, line)
    }

    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        if let Some(error) = self.fail_next.take() {
            return Err(error);
        }
        self.net.lock().close(self.id, conn)
    }

    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.net.lock().queued_bytes(self.id, conn)
    }
}

impl Drop for FakeLink {
    fn drop(&mut self) {
        self.net.lock().drop_link(self.id);
    }
}

/// A client that never reads what the host sends it.
pub struct StalledPeer {
    net: FakeNetwork,
    id: LinkId,
}

impl Drop for StalledPeer {
    fn drop(&mut self) {
        self.net.lock().drop_link(self.id);
    }
}

/// The contract fixture: a fresh network per test.
impl LinkFixture for FakeNetwork {
    type Host = FakeLink;
    type Client = FakeLink;
    type Stalled = StalledPeer;

    #[allow(clippy::unwrap_used)]
    fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> FakeLink {
        FakeNetwork::listen(self, sink).unwrap()
    }

    #[allow(clippy::unwrap_used)]
    fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (FakeLink, ConnId) {
        FakeNetwork::connect(self, sink).unwrap()
    }

    #[allow(clippy::unwrap_used)]
    fn stalled_peer(&mut self) -> StalledPeer {
        FakeNetwork::stalled_peer(self).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::link::Events;

    mod contract {
        use super::FakeNetwork;

        crate::link_contract!(FakeNetwork::new());
    }

    #[test]
    fn fail_next_fails_only_the_next_call() {
        let net = FakeNetwork::new();
        let (host_sink, host_events) = Events::channel();
        let (client_sink, _client_events) = Events::channel();
        let _host = net.listen(host_sink).unwrap();
        let (mut client, conn) = net.connect(client_sink).unwrap();
        let host_conn = match host_events.wait_for(|_| true, std::time::Duration::ZERO)[..] {
            [LinkEvent::Connected { conn }] => conn,
            _ => unreachable!(),
        };
        client.fail_next(LinkError::Io("cable cut".into()));

        assert_eq!(
            client.send(conn, b"lost"),
            Err(LinkError::Io("cable cut".into()))
        );
        client.send(conn, b"kept").unwrap();
        assert_eq!(host_events.lines_of(host_conn, 1), [b"kept".to_vec()]);
    }

    /// A host and one paced client, with the client's conn on both ends.
    fn paced() -> (FakeLink, Events, FakeLink, ConnId, ConnId, FakeNetwork) {
        let net = FakeNetwork::new();
        let (host_sink, host_events) = Events::channel();
        let (client_sink, client_events) = Events::channel();
        let host = net.listen(host_sink).unwrap();
        let (client, conn) = net.connect_paced(client_sink).unwrap();
        let host_conn = match host_events.wait_for(|_| true, std::time::Duration::ZERO)[..] {
            [LinkEvent::Connected { conn }] => conn,
            _ => unreachable!(),
        };
        (host, client_events, client, conn, host_conn, net)
    }

    #[test]
    fn a_paced_client_reads_only_what_it_is_given_in_order() {
        let (mut host, events, _client, conn, host_conn, net) = paced();
        for line in [&b"one"[..], b"two", b"three"] {
            host.send(host_conn, line).unwrap();
        }
        assert_eq!(host.queued_bytes(host_conn), Some(11));
        events.assert_quiet(std::time::Duration::ZERO);

        assert_eq!(net.deliver(conn, 6), 6);
        assert_eq!(host.queued_bytes(host_conn), Some(5));
        assert_eq!(events.lines_of(conn, 2), [b"one".to_vec(), b"two".to_vec()]);
        // At least one line, even past the budget.
        assert_eq!(net.deliver(conn, 1), 5);
        assert_eq!(events.lines_of(conn, 1), [b"three".to_vec()]);
        assert_eq!(host.queued_bytes(host_conn), Some(0));
    }

    #[test]
    fn a_paced_client_that_falls_too_far_behind_is_a_slow_consumer() {
        let (mut host, _events, _client, _conn, host_conn, _net) = paced();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..=MAX_QUEUED / chunk.len() {
            host.send(host_conn, &chunk).unwrap();
        }
        assert_eq!(host.queued_bytes(host_conn), None);
    }

    #[test]
    fn connecting_with_no_host_listening_fails() {
        let (sink, _events) = Events::channel();
        assert!(matches!(
            FakeNetwork::new().connect(sink),
            Err(LinkError::Io(_))
        ));
    }
}
