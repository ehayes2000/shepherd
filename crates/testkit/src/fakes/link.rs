//! An in-memory `Link`. A [`FakeNetwork`] stands in for the transport: one host listens on it, and clients
//! connect through it. Each conn is a pair of ends, one per link, and every end has its own [`ConnId`].
//!
//! Events are sent synchronously, from inside the call that caused them, while the network's lock is held, so
//! sinks must not call back into a link. A [`StalledPeer`] never reads: lines sent to it pile up in its queue
//! until [`MAX_QUEUED`] is passed, which disconnects it as a slow consumer, just like a socket peer that stops
//! reading.

use std::collections::HashMap;
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
    peer: ConnId,
    /// Bytes sent to this end and not yet read. Only a stalled peer leaves bytes unread.
    unread: usize,
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
            End {
                owner: host,
                sink: Some(host_sink.clone()),
                peer: client_end,
                unread: 0,
            },
        );
        self.ends.insert(
            client_end,
            End {
                owner: link,
                sink,
                peer: host_end,
                unread: 0,
            },
        );
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
            Some(sink) => sink.send(LinkEvent::Line {
                conn: peer,
                line: line.to_vec(),
            }),
            None if to.unread > 0 && to.unread + line.len() > MAX_QUEUED => {
                self.disconnect(
                    conn,
                    DisconnectReason::SlowConsumer,
                    DisconnectReason::PeerGone,
                );
            }
            None => to.unread += line.len(),
        }
        Ok(())
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
        let (id, conn) = self.lock().connect(Some(sink))?;
        Ok((FakeLink::new(self.clone(), id), conn))
    }

    /// Connects a peer that never reads. Dropping it is an abrupt disconnect.
    pub fn stalled_peer(&self) -> Result<StalledPeer, LinkError> {
        let (id, _) = self.lock().connect(None)?;
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

    #[test]
    fn connecting_with_no_host_listening_fails() {
        let (sink, _events) = Events::channel();
        assert!(matches!(
            FakeNetwork::new().connect(sink),
            Err(LinkError::Io(_))
        ));
    }
}
