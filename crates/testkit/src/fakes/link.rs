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
//! process's threads. [`Link::flush_closed`] waits for those deliveries. Like the real adapters' writers, it gives up
//! on a closed conn whose peer still hasn't read everything [`CLOSE_FLUSH`] after the close: the peer is cut off
//! (`PeerGone`), and the give-up is reported (`TimedOut`) to the call it happened in, then forgotten. It gives up only
//! from inside `flush_closed`, so until someone waits, a paced peer can still read a closed conn's lines however late.
//! [`FakeNetwork::wait_until_flushing`] lets a test read only once someone is waiting.
//!
//! A paused end ([`Link::pause_reading`]) holds what is sent to it like a paced one, until it is resumed. A paced end
//! can also be shown the first line it is reading as on its way, a few bytes at a time ([`FakeNetwork::trickle`]):
//! [`Link::drained_bytes`] rises while [`Link::queued_bytes`] doesn't fall, like one huge line over a slow link.
//!
//! Machines (decision 0019): a link made with [`FakeNetwork::listen_as`] is the host of a machine. It accepts conns
//! other machines [open](Link::open) to it from the machines it [trusts](FakeNetwork::trust), refusing the rest, and
//! opens conns to other machines' hosts; each end of such a conn names the other's machine ([`Link::peer`]). Opening
//! a conn to a machine nobody hosts ends it as `PeerGone`. Dropping a machine's link is that machine going away. A
//! machine can be made to read its conns like a paced client ([`FakeNetwork::pace`]), so a test plays its link's
//! reader: [`FakeNetwork::read_as`] delivers a chunk to each of its conns that isn't paused, as a reader that looks
//! at the pause between chunks does.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    CLOSE_FLUSH, ConnId, DisconnectReason, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink,
    MAX_QUEUED,
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
    /// Holds lines until its owner resumes reading it.
    paused: bool,
    peer: ConnId,
    /// The machine at the other end, for a conn between machines.
    with: Option<MachineId>,
    /// Lines sent to a paced or paused end and not yet delivered.
    pending: VecDeque<Vec<u8>>,
    /// Bytes sent to this end and not yet read. Only a stalled, paced or paused peer leaves bytes unread.
    unread: usize,
    /// Bytes this end has read.
    read: u64,
    /// Bytes of the first pending line already on their way ([`FakeNetwork::trickle`]), short of all of it.
    partial: usize,
    /// The link that closed this end's conn while it still had bytes unread. The end stays until it has read them.
    closed_by: Option<LinkId>,
    /// When that close happened, so [`Link::flush_closed`] gives up on it [`CLOSE_FLUSH`] later.
    closed_at: Option<Instant>,
}

impl End {
    fn new(owner: LinkId, sink: Option<Arc<dyn LinkEventSink>>, peer: ConnId) -> Self {
        Self {
            owner,
            sink,
            paced: false,
            paused: false,
            peer,
            with: None,
            pending: VecDeque::new(),
            unread: 0,
            read: 0,
            partial: 0,
            closed_by: None,
            closed_at: None,
        }
    }

    /// Whether lines sent to it are delivered at once.
    fn reading(&self) -> bool {
        self.sink.is_some() && !self.paced && !self.paused
    }
}

/// A machine's host: its link and sink, and the machines it lets in.
struct Machine {
    link: LinkId,
    sink: Arc<dyn LinkEventSink>,
    trusted: HashSet<MachineId>,
}

#[derive(Default)]
struct Net {
    /// The listening host's link and sink.
    host: Option<(LinkId, Arc<dyn LinkEventSink>)>,
    /// Machines' hosts, by machine.
    machines: HashMap<MachineId, Machine>,
    /// What each machine trusts, kept across its host going and coming back.
    trust: HashMap<MachineId, HashSet<MachineId>>,
    /// Machines that read their conns only through [`FakeNetwork::read_as`].
    paced: HashSet<MachineId>,
    /// The listening host reads its clients only through [`FakeNetwork::read_host`].
    host_paced: bool,
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
        let mut host_side = End::new(host, Some(host_sink.clone()), client_end);
        host_side.paced = self.host_paced;
        self.ends.insert(host_end, host_side);
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
            Some(sink) if to.reading() => {
                to.read += line.len() as u64;
                sink.send(LinkEvent::Line {
                    conn: peer,
                    line: line.to_vec(),
                })
            }
            _ if to.unread > 0 && to.unread + line.len() > MAX_QUEUED => {
                self.disconnect(
                    conn,
                    DisconnectReason::SlowConsumer,
                    DisconnectReason::PeerGone,
                );
            }
            _ => {
                to.unread += line.len();
                if to.paced || to.paused {
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

    /// What `conn`'s peer has read, and what of the line it is reading is on its way.
    fn drained_bytes(&self, link: LinkId, conn: ConnId) -> Option<u64> {
        let end = self.ends.get(&conn).filter(|end| end.owner == link)?;
        Some(
            self.ends
                .get(&end.peer)
                .map_or(0, |peer| peer.read + peer.partial as u64),
        )
    }

    fn peer(&self, link: LinkId, conn: ConnId) -> Result<Option<MachineId>, LinkError> {
        let end = self
            .ends
            .get(&conn)
            .filter(|end| end.owner == link)
            .ok_or(LinkError::UnknownConn)?;
        Ok(end.with.clone())
    }

    fn pause_reading(&mut self, link: LinkId, conn: ConnId, paused: bool) -> Result<(), LinkError> {
        let end = self.end(link, conn)?;
        end.paused = paused;
        if !end.reading() {
            return Ok(());
        }
        self.deliver(conn, usize::MAX);
        Ok(())
    }

    /// Opens a conn from `link`, a machine's host, to `peer`'s host. Everything it causes is sent before it returns.
    fn open(&mut self, link: LinkId, peer: &MachineId) -> Result<ConnId, LinkError> {
        let (me, sink) = self
            .machines
            .iter()
            .find(|(_, m)| m.link == link)
            .map(|(id, m)| (id.clone(), m.sink.clone()))
            .ok_or(LinkError::NoPeers)?;
        let here = ConnId::new(self.next_id());
        let refused = match self.machines.get(peer) {
            None => Some(DisconnectReason::PeerGone),
            Some(host) if !host.trusted.contains(&me) => Some(DisconnectReason::Refused),
            Some(_) => None,
        };
        if let Some(reason) = refused {
            sink.send(LinkEvent::Disconnected { conn: here, reason });
            return Ok(here);
        }
        let Some(host) = self.machines.get(peer) else {
            return Ok(here);
        };
        let (host_link, host_sink) = (host.link, host.sink.clone());
        let there = ConnId::new(self.next_id());
        let mut theirs = End::new(host_link, Some(host_sink.clone()), here);
        theirs.paced = self.paced.contains(peer);
        let mut ours = End::new(link, Some(sink.clone()), there);
        ours.paced = self.paced.contains(&me);
        theirs.with = Some(me);
        ours.with = Some(peer.clone());
        self.ends.insert(there, theirs);
        self.ends.insert(here, ours);
        host_sink.send(LinkEvent::Connected { conn: there });
        sink.send(LinkEvent::Connected { conn: here });
        Ok(here)
    }

    /// Delivers lines pending for the paced or paused end `conn`, oldest first, up to `max_bytes` (always at least
    /// one line if any is pending), unless it is paused. Returns the bytes delivered.
    fn deliver(&mut self, conn: ConnId, max_bytes: usize) -> usize {
        let Some(end) = self.ends.get_mut(&conn).filter(|end| !end.paused) else {
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
            end.read += line.len() as u64;
            end.partial = 0;
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

    /// Lets each paced conn of `link` that isn't paused read up to `max_bytes`. Returns the bytes read.
    fn read_link(&mut self, link: LinkId, max_bytes: usize) -> usize {
        let mut conns: Vec<ConnId> = self
            .ends
            .iter()
            .filter(|(_, end)| end.owner == link && end.paced)
            .map(|(&conn, _)| conn)
            .collect();
        conns.sort();
        conns
            .into_iter()
            .map(|conn| self.deliver(conn, max_bytes))
            .sum()
    }

    /// Moves `bytes` more of the first line pending for `conn` along, never all of it.
    fn trickle(&mut self, conn: ConnId, bytes: usize) {
        if let Some(end) = self.ends.get_mut(&conn)
            && let Some(line) = end.pending.front()
        {
            end.partial = (end.partial + bytes).min(line.len().saturating_sub(1));
        }
    }

    /// A peer with bytes still unread keeps its end, delivering, until it has read them (see the module docs).
    fn close(&mut self, link: LinkId, conn: ConnId) -> Result<(), LinkError> {
        let peer = self.end(link, conn)?.peer;
        match self.ends.get_mut(&peer) {
            Some(to) if to.unread > 0 => {
                to.closed_by = Some(link);
                to.closed_at = Some(Instant::now());
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

    /// Gives up on each conn `link` closed that is still delivering [`CLOSE_FLUSH`] after its close, as of `now`: its
    /// peer is cut off. Returns whether it gave up on any, and when the next one is due.
    fn give_up(&mut self, link: LinkId, now: Instant) -> (bool, Option<Instant>) {
        let due = |end: &End| end.closed_at.map(|at| at + CLOSE_FLUSH);
        let mut expired: Vec<ConnId> = self
            .ends
            .iter()
            .filter(|(_, end)| end.closed_by == Some(link) && due(end).is_some_and(|d| d <= now))
            .map(|(&conn, _)| conn)
            .collect();
        expired.sort();
        let gave_up = !expired.is_empty();
        for conn in expired {
            self.disconnect(conn, DisconnectReason::PeerGone, DisconnectReason::PeerGone);
        }
        let next = self
            .ends
            .values()
            .filter(|end| end.closed_by == Some(link))
            .filter_map(due)
            .min();
        (gave_up, next)
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
        self.machines.retain(|_, m| m.link != link);
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

    /// Starts the host of machine `machine`, trusting nobody yet (unless it trusted machines before, as a host
    /// restarting does). Fails if that machine has a host already.
    pub fn listen_as(
        &self,
        machine: MachineId,
        sink: Arc<dyn LinkEventSink>,
    ) -> Result<FakeLink, LinkError> {
        let mut net = self.lock();
        if net.machines.contains_key(&machine) {
            return Err(LinkError::Io("address in use".into()));
        }
        let id = net.next_id();
        let trusted = net.trust.get(&machine).cloned().unwrap_or_default();
        net.machines.insert(
            machine.clone(),
            Machine {
                link: id,
                sink,
                trusted,
            },
        );
        drop(net);
        let mut link = FakeLink::new(self.clone(), id);
        link.machine = Some(machine);
        Ok(link)
    }

    /// Lets `peer` open conns to `host`'s host, from now on.
    pub fn trust(&self, host: &MachineId, peer: &MachineId) {
        let mut net = self.lock();
        net.trust
            .entry(host.clone())
            .or_default()
            .insert(peer.clone());
        if let Some(m) = net.machines.get_mut(host) {
            m.trusted.insert(peer.clone());
        }
    }

    /// Makes `machine`'s host read the conns it opens or accepts from now on only through [`FakeNetwork::read_as`].
    pub fn pace(&self, machine: &MachineId) {
        self.lock().paced.insert(machine.clone());
    }

    /// Lets each paced conn of `machine`'s host that isn't paused read up to `max_bytes` (at least one line, if any
    /// is waiting). Returns the bytes read.
    pub fn read_as(&self, machine: &MachineId, max_bytes: usize) -> usize {
        let mut net = self.lock();
        match net.machines.get(machine).map(|m| m.link) {
            Some(link) => net.read_link(link, max_bytes),
            None => 0,
        }
    }

    /// Makes the listening host read the clients that connect from now on only through [`FakeNetwork::read_host`].
    pub fn pace_host(&self) {
        self.lock().host_paced = true;
    }

    /// [`FakeNetwork::read_as`] for the listening host's clients.
    pub fn read_host(&self, max_bytes: usize) -> usize {
        let mut net = self.lock();
        match net.host.as_ref().map(|(link, _)| *link) {
            Some(link) => net.read_link(link, max_bytes),
            None => 0,
        }
    }

    /// What the conns of `machine`'s host haven't read yet: what the other ends have queued for it.
    pub fn unread_by(&self, machine: &MachineId) -> usize {
        let net = self.lock();
        let Some(link) = net.machines.get(machine).map(|m| m.link) else {
            return 0;
        };
        net.ends
            .values()
            .filter(|end| end.owner == link)
            .map(|end| end.unread)
            .sum()
    }

    /// What the end `conn` hasn't read yet: what its other end has queued for it.
    pub fn unread(&self, conn: ConnId) -> usize {
        self.lock().ends.get(&conn).map_or(0, |end| end.unread)
    }

    /// Moves `bytes` more of the first line waiting for the paced end `conn` along without delivering it, as a slow
    /// link does with one huge line: the sender's [`Link::drained_bytes`] rises, its [`Link::queued_bytes`] doesn't
    /// fall. It never moves all of it: [`FakeNetwork::deliver`] delivers it.
    pub fn trickle(&self, conn: ConnId, bytes: usize) {
        self.lock().trickle(conn, bytes);
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

    /// Waits until every conn `link` closed has finished delivering or been given up on, or `deadline` passes.
    /// `Flushed` only if it gave up on none.
    fn flush_closed(&self, link: LinkId, deadline: Instant) -> FlushOutcome {
        let mut net = self.0.net.lock().unwrap_or_else(PoisonError::into_inner);
        net.flushing += 1;
        self.0.changed.notify_all();
        let mut gave_up = false;
        let outcome = loop {
            let now = Instant::now();
            let (gave_up_now, next_give_up) = net.give_up(link, now);
            gave_up |= gave_up_now;
            if net.flushed(link) {
                break match gave_up {
                    true => FlushOutcome::TimedOut,
                    false => FlushOutcome::Flushed,
                };
            }
            if deadline <= now {
                break FlushOutcome::TimedOut;
            }
            let until = next_give_up.map_or(deadline, |due| due.min(deadline));
            net = self
                .0
                .changed
                .wait_timeout(net, until.saturating_duration_since(now))
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
    /// The machine it hosts, if it is a machine's host.
    machine: Option<MachineId>,
    fail_next: Option<LinkError>,
}

impl FakeLink {
    fn new(net: FakeNetwork, id: LinkId) -> Self {
        Self {
            net,
            id,
            machine: None,
            fail_next: None,
        }
    }

    /// The machine it hosts, for a link made with [`FakeNetwork::listen_as`].
    pub fn machine(&self) -> Option<&MachineId> {
        self.machine.as_ref()
    }

    /// Makes the next `send`, `close` or `open` on this link fail with `error`, doing nothing else.
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

    fn flush_closed(&mut self, within: Duration) -> FlushOutcome {
        self.net.flush_closed(self.id, Instant::now() + within)
    }

    fn peer(&self, conn: ConnId) -> Result<Option<MachineId>, LinkError> {
        self.net.lock().peer(self.id, conn)
    }

    fn drained_bytes(&self, conn: ConnId) -> Option<u64> {
        self.net.lock().drained_bytes(self.id, conn)
    }

    fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError> {
        self.net.lock().pause_reading(self.id, conn, paused)
    }

    fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError> {
        if let Some(error) = self.fail_next.take() {
            return Err(error);
        }
        self.net.lock().open(self.id, peer)
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

/// The contract fixture for links between machines: a host machine, and each client a machine of its own that
/// [opens](Link::open) its conn to it.
pub struct FakeMachines {
    net: FakeNetwork,
    host: MachineId,
    next: u64,
}

impl FakeMachines {
    pub fn new() -> Self {
        Self {
            net: FakeNetwork::new(),
            host: MachineId::new("host"),
            next: 0,
        }
    }

    /// A fresh machine's host, trusted by the host machine if `trusted`, and its conn to the host machine.
    #[allow(clippy::unwrap_used)]
    fn client(&mut self, sink: Arc<dyn LinkEventSink>, trusted: bool) -> (FakeLink, ConnId) {
        self.next += 1;
        let me = MachineId::new(format!("client{}", self.next));
        if trusted {
            self.net.trust(&self.host, &me);
        }
        let mut link = self.net.listen_as(me, sink).unwrap();
        let conn = link.open(&self.host).unwrap();
        (link, conn)
    }
}

impl Default for FakeMachines {
    fn default() -> Self {
        Self::new()
    }
}

/// A machine's conn that never reads (it is paused), and goes when dropped.
pub struct StalledMachine {
    _link: FakeLink,
}

impl LinkFixture for FakeMachines {
    type Host = FakeLink;
    type Client = FakeLink;
    type Stalled = StalledMachine;

    #[allow(clippy::unwrap_used)]
    fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> FakeLink {
        self.net.listen_as(self.host.clone(), sink).unwrap()
    }

    fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (FakeLink, ConnId) {
        self.client(sink, true)
    }

    #[allow(clippy::unwrap_used)]
    fn stalled_peer(&mut self) -> StalledMachine {
        let (sink, _) = crate::contracts::link::Events::channel();
        let (mut link, conn) = self.client(sink, true);
        link.pause_reading(conn, true).unwrap();
        StalledMachine { _link: link }
    }

    fn machine_of(&self, client: &FakeLink) -> Option<MachineId> {
        client.machine().cloned()
    }

    fn host_machine(&self) -> Option<MachineId> {
        Some(self.host.clone())
    }

    fn untrusted(&mut self, sink: Arc<dyn LinkEventSink>) -> Option<(FakeLink, ConnId)> {
        Some(self.client(sink, false))
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

    mod machines_contract {
        use super::FakeMachines;

        crate::link_contract!(FakeMachines::new());
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
    fn a_line_trickling_to_a_paced_client_is_drained_but_still_queued_until_read() {
        let (mut host, events, _client, conn, host_conn, net) = paced();
        host.send(host_conn, b"0123456789").unwrap();
        net.trickle(conn, 4);
        assert_eq!(host.drained_bytes(host_conn), Some(4));
        assert_eq!(host.queued_bytes(host_conn), Some(10));
        // Never all of it: only reading it delivers the last byte.
        net.trickle(conn, 100);
        assert_eq!(host.drained_bytes(host_conn), Some(9));
        events.assert_quiet(Duration::ZERO);
        net.deliver(conn, usize::MAX);
        assert_eq!(host.drained_bytes(host_conn), Some(10));
        assert_eq!(host.queued_bytes(host_conn), Some(0));
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
        assert_eq!(host.flush_closed(Duration::ZERO), FlushOutcome::TimedOut);
        net.deliver(conn, usize::MAX);
        assert_eq!(
            events.disconnect_of(conn),
            (vec![b"bye".to_vec()], DisconnectReason::PeerGone)
        );
        assert_eq!(host.flush_closed(Duration::ZERO), FlushOutcome::Flushed);

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
