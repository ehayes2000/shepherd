//! An in-memory `Link`. A [`FakeNetwork`] stands in for the transport: one host listens on it, and clients
//! connect through it. Each conn is a pair of ends, one per link, and every end has its own [`ConnId`].
//!
//! Events are sent synchronously, from inside the call that caused them, while the network's lock is held, so
//! sinks must not call back into a link. A [`StalledPeer`] never reads: lines sent to it pile up in its queue
//! until [`MAX_QUEUED`] is passed, which disconnects it as a slow consumer, just like a socket peer that stops
//! reading. A paced client ([`FakeNetwork::connect_paced`]) reads only when the test says so
//! ([`FakeNetwork::deliver`]), so a test can play a slow reader and watch [`Link::queued_bytes`].
//!
//! Closing a conn whose peer has lines it hasn't read leaves the peer's end open, still delivering, until it has read
//! them (then it gets `PeerGone`) or the closing link is dropped, which cuts the delivery off like an exec ending a
//! process's threads. [`Link::flush_closed`] waits for those deliveries. Nothing here gives up on a peer by itself
//! (the unix adapter gives up after `CLOSE_FLUSH`), so a stalled peer's closed conn stays undelivered until the
//! deadline. [`FakeNetwork::wait_until_flushing`] lets a test read only once someone is waiting.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    ConnId, DisconnectReason, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED,
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
    /// The link that closed this end's conn while it still had bytes unread. The end stays until it has read them.
    closed_by: Option<LinkId>,
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
            closed_by: None,
        }
    }
}

#[derive(Default)]
struct Net {
    /// The listening host's link and sink.
    host: Option<(LinkId, Arc<dyn LinkEventSink>)>,
    ends: HashMap<ConnId, End>,
    next_id: u64,
    /// How many callers are inside [`Link::flush_closed`] right now.
    flushing: usize,
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
        if end.closed_by.is_some() && end.unread == 0 {
            // Its conn was closed and it has now read everything: the close is done.
            self.disconnect(conn, DisconnectReason::PeerGone, DisconnectReason::PeerGone);
        }
        delivered
    }

    /// A peer with bytes still unread keeps its end, delivering, until it has read them (see the module docs).
    fn close(&mut self, link: LinkId, conn: ConnId) -> Result<(), LinkError> {
        let peer = self.end(link, conn)?.peer;
        match self.ends.get_mut(&peer) {
            Some(to) if to.unread > 0 => {
                to.closed_by = Some(link);
                if let Some(sink) = self.ends.remove(&conn).and_then(|this| this.sink) {
                    sink.send(LinkEvent::Disconnected {
                        conn,
                        reason: DisconnectReason::Closed,
                    });
                }
            }
            _ => self.disconnect(conn, DisconnectReason::Closed, DisconnectReason::PeerGone),
        }
        Ok(())
    }

    /// Whether every conn `link` closed has finished delivering.
    fn flushed(&self, link: LinkId) -> bool {
        !self.ends.values().any(|end| end.closed_by == Some(link))
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

    /// Disconnects every conn of `link` without flushing, as when its process dies, and cuts off what the conns it
    /// closed were still delivering.
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
        // The peers of those closes: their side of the conn is gone.
        let mut cut_off: Vec<ConnId> = self
            .ends
            .iter()
            .filter(|(_, end)| end.closed_by == Some(link))
            .map(|(&conn, _)| conn)
            .collect();
        cut_off.sort();
        for conn in cut_off {
            self.disconnect(conn, DisconnectReason::PeerGone, DisconnectReason::PeerGone);
        }
    }
}

/// The transport the fake links share. Cheap to clone; clones are the same network.
#[derive(Clone, Default)]
pub struct FakeNetwork(Arc<Shared>);

#[derive(Default)]
struct Shared {
    net: Mutex<Net>,
    /// Notified whenever the network's lock is released, so waiters can look again.
    changed: Condvar,
}

/// The network locked. Releasing it wakes whoever waits for a change.
struct Locked<'a> {
    net: MutexGuard<'a, Net>,
    changed: &'a Condvar,
}

impl std::ops::Deref for Locked<'_> {
    type Target = Net;

    fn deref(&self) -> &Net {
        &self.net
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut Net {
        &mut self.net
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        self.changed.notify_all();
    }
}

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

    /// Whether the other side closed `conn`'s conn while `conn` still had lines to read: they are delivered as it
    /// reads them, and its `Disconnected` comes after.
    pub fn closed_by_peer(&self, conn: ConnId) -> bool {
        self.lock()
            .ends
            .get(&conn)
            .is_some_and(|end| end.closed_by.is_some())
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

    /// Waits until some link is inside [`Link::flush_closed`], or `timeout` passes. Returns whether one is: a test
    /// playing a slow reader reads only then, so a caller that never waits for the flush never gets it.
    pub fn wait_until_flushing(&self, timeout: Duration) -> bool {
        let net = self.0.net.lock().unwrap_or_else(PoisonError::into_inner);
        let (net, _) = self
            .0
            .changed
            .wait_timeout_while(net, timeout, |net| net.flushing == 0)
            .unwrap_or_else(PoisonError::into_inner);
        net.flushing > 0
    }

    /// Waits until every conn `link` closed has finished delivering, or `deadline` passes.
    fn flush_closed(&self, link: LinkId, deadline: Instant) -> FlushOutcome {
        let mut net = self.0.net.lock().unwrap_or_else(PoisonError::into_inner);
        net.flushing += 1;
        self.0.changed.notify_all();
        let outcome = loop {
            if net.flushed(link) {
                break FlushOutcome::Flushed;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break FlushOutcome::TimedOut;
            }
            net = self
                .0
                .changed
                .wait_timeout(net, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        net.flushing -= 1;
        outcome
    }

    fn lock(&self) -> Locked<'_> {
        Locked {
            net: self.0.net.lock().unwrap_or_else(PoisonError::into_inner),
            changed: &self.0.changed,
        }
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

    fn flush_closed(&mut self, deadline: Instant) -> FlushOutcome {
        self.net.flush_closed(self.id, deadline)
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
    fn a_paced_client_reads_what_was_queued_before_a_close_unless_the_host_goes_first() {
        let (mut host, events, _client, conn, host_conn, net) = paced();
        host.send(host_conn, b"bye").unwrap();
        host.close(host_conn).unwrap();
        assert_eq!(host.flush_closed(Instant::now()), FlushOutcome::TimedOut);
        net.deliver(conn, usize::MAX);
        assert_eq!(
            events.disconnect_of(conn),
            (vec![b"bye".to_vec()], DisconnectReason::PeerGone)
        );
        assert_eq!(host.flush_closed(Instant::now()), FlushOutcome::Flushed);

        let (mut host, events, _client, conn, host_conn, _net) = paced();
        host.send(host_conn, b"lost").unwrap();
        host.close(host_conn).unwrap();
        drop(host);
        assert_eq!(
            events.disconnect_of(conn),
            (vec![], DisconnectReason::PeerGone)
        );
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
