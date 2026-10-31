//! The `Link` contract. Every case needs a host end and one or more client ends, so links are built through a
//! [`LinkFixture`]: the fake's fixture wires them together in memory, the unix adapter's binds a socket in a
//! tempdir. Transport-specific behaviour (socket files) is tested next to the adapter.
//!
//! Run it with [`link_contract!`](crate::link_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Link` contract case. `$fixture` is an expression building a fresh
/// [`LinkFixture`] for each test, e.g. `shepherd_testkit::link_contract!(UnixFixture::new());`.
///
/// New cases go in the list below.
#[macro_export]
macro_rules! link_contract {
    ($fixture:expr) => {
        $crate::link_contract!(@cases $fixture;
            lines_reach_the_host_in_order,
            lines_reach_the_client_in_order,
            ten_thousand_lines_arrive_in_order_both_ways,
            several_clients_at_once_each_get_their_own_lines,
            host_close_disconnects_both_ends,
            client_close_disconnects_both_ends,
            lines_queued_before_host_close_are_delivered,
            lines_queued_before_client_close_are_delivered,
            disconnect_is_sent_once_and_last,
            closed_conn_is_unknown,
            unknown_conn_is_an_error,
            line_containing_a_newline_is_invalid,
            dropping_a_client_is_peer_gone_on_the_host,
            dropping_the_host_is_peer_gone_on_every_client,
            peer_that_dies_abruptly_is_peer_gone,
            peer_that_dies_right_after_connecting_is_announced_then_gone,
            peer_that_never_reads_is_disconnected_as_slow_consumer,
            no_lines_arrive_after_close_returns,
            line_over_the_limit_is_a_protocol_error,
            other_conns_survive_a_disconnect,
            queued_bytes_rise_while_the_peer_does_not_read,
            queued_bytes_fall_to_zero_once_the_peer_has_read_everything,
            queued_bytes_of_an_unknown_or_closed_conn_is_none,
            flush_closed_with_nothing_closed_is_flushed,
            closed_conn_is_flushed_once_its_peer_has_read_everything,
            flush_closed_times_out_while_a_closed_peer_does_not_read,
            closed_conn_whose_peer_goes_away_is_flushed,
            a_give_up_is_reported_once_and_holds_no_later_flush,
            each_end_names_the_machine_the_other_is,
            the_peer_of_an_unknown_or_gone_conn_is_an_error_not_local,
            opening_a_conn_to_a_machine_nobody_hosts,
            a_machine_the_host_does_not_trust_is_refused_and_never_announced,
            a_paused_conn_emits_no_lines_until_resumed_then_all_in_order,
            the_peer_of_a_paused_conn_queues_what_it_sends,
            a_paused_conn_still_closes,
            pausing_an_unknown_conn_is_an_error,
            drained_bytes_rise_as_the_peer_reads,
            drained_bytes_of_an_unknown_or_closed_conn_is_none,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::link::$case($crate::contracts::link::LinkHarness::new($fixture));
            }
        )*
    };
}

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    CLOSE_FLUSH, ConnId, DisconnectReason, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink,
    MAX_QUEUED,
};
use shepherd_core::wire::MAX_LINE;

const TIMEOUT: Duration = Duration::from_secs(10);
/// How long to watch for stray events after a disconnect. Only used to assert that nothing else arrives.
const QUIET: Duration = Duration::from_millis(300);

/// Builds the ends of a link. Each test gets a fresh fixture.
pub trait LinkFixture {
    /// `Send`, so a case can drive it from another thread and notice if it blocks.
    type Host: Link + Send + 'static;
    type Client: Link;
    /// A connection to the host that never reads what it is sent, and goes away abruptly when dropped (for a
    /// socket: its file descriptor is closed with no shutdown).
    type Stalled;

    /// The host end, listening. It emits `Connected` for each client.
    fn listen(&mut self, sink: Arc<dyn LinkEventSink>) -> Self::Host;
    /// A client end connected to the host from [`listen`](Self::listen), and its conn.
    fn connect(&mut self, sink: Arc<dyn LinkEventSink>) -> (Self::Client, ConnId);
    fn stalled_peer(&mut self) -> Self::Stalled;
    /// The machine the host's link should name for `client`'s conn ([`Link::peer`]): `None` for a local transport.
    fn machine_of(&self, client: &Self::Client) -> Option<MachineId>;
    /// The machine a client's link should name for its conn to the host: `None` for a local transport.
    fn host_machine(&self) -> Option<MachineId>;
    /// A client end on a machine the host doesn't trust, and its conn, which the host refuses. `None` for a transport
    /// with nothing to refuse (a local socket only this machine's user can reach).
    fn untrusted(&mut self, sink: Arc<dyn LinkEventSink>) -> Option<(Self::Client, ConnId)>;
    /// Whether the host's link [opens](Link::open) conns to other machines: by default, if it is a machine's.
    fn dials(&self) -> bool {
        self.host_machine().is_some()
    }
    /// A machine id nobody hosts, for links that dial: one the transport takes as a machine's.
    fn nobody(&self) -> MachineId {
        MachineId::new("nobody")
    }
}

/// A sink that forwards events to an mpsc channel. Send errors (receiver gone) are ignored.
pub struct ChannelSink(pub Sender<LinkEvent>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = self.0.send(event);
    }
}

/// The receiving end of one link's sink.
pub struct Events(Receiver<LinkEvent>);

impl Events {
    pub fn channel() -> (Arc<dyn LinkEventSink>, Self) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(ChannelSink(tx)), Self(rx))
    }

    /// Returns every event received up to and including the first one matching `predicate`.
    /// Panics if none matches within `timeout`.
    pub fn wait_for(
        &self,
        mut predicate: impl FnMut(&LinkEvent) -> bool,
        timeout: Duration,
    ) -> Vec<LinkEvent> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.0.recv_timeout(left) {
                Ok(event) => {
                    let done = predicate(&event);
                    seen.push(event);
                    if done {
                        return seen;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "no matching link event within {timeout:?}; saw {}",
                        summary(&seen)
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("link event sink dropped; saw {}", summary(&seen))
                }
            }
        }
    }

    /// Waits until each `(conn, n)` has received `n` lines, and returns them per conn. Panics if one of those
    /// conns disconnects first, or gets more than `n`.
    pub fn lines(&self, expected: &[(ConnId, usize)]) -> HashMap<ConnId, Vec<Vec<u8>>> {
        let want: HashMap<ConnId, usize> = expected.iter().copied().collect();
        let mut got: HashMap<ConnId, Vec<Vec<u8>>> =
            want.keys().map(|&c| (c, Vec::new())).collect();
        let done =
            |got: &HashMap<ConnId, Vec<Vec<u8>>>| want.iter().all(|(c, &n)| got[c].len() >= n);
        if done(&got) {
            return got;
        }
        self.wait_for(
            |e| {
                match e {
                    LinkEvent::Line { conn, line } if want.contains_key(conn) => {
                        let lines = got.get_mut(conn).unwrap();
                        assert!(lines.len() < want[conn], "{conn}: more lines than sent");
                        lines.push(line.clone());
                    }
                    LinkEvent::Disconnected { conn, reason } if want.contains_key(conn) => {
                        panic!(
                            "{conn} disconnected ({reason:?}) after {} of {} lines",
                            got[conn].len(),
                            want[conn]
                        )
                    }
                    _ => {}
                }
                done(&got)
            },
            TIMEOUT,
        );
        got
    }

    /// The next `n` lines for `conn`.
    pub fn lines_of(&self, conn: ConnId, n: usize) -> Vec<Vec<u8>> {
        self.lines(&[(conn, n)]).remove(&conn).unwrap()
    }

    /// Waits for `conn`'s `Disconnected`. Returns the lines `conn` received first, and the reason.
    pub fn disconnect_of(&self, conn: ConnId) -> (Vec<Vec<u8>>, DisconnectReason) {
        let events = self.wait_for(
            |e| matches!(e, LinkEvent::Disconnected { conn: c, .. } if *c == conn),
            TIMEOUT,
        );
        let mut lines = Vec::new();
        let mut reason = None;
        for event in events {
            match event {
                LinkEvent::Line { conn: c, line } if c == conn => lines.push(line),
                LinkEvent::Disconnected { conn: c, reason: r } if c == conn => reason = Some(r),
                _ => {}
            }
        }
        (lines, reason.unwrap())
    }

    /// The events that have already arrived, without waiting for more.
    pub fn ready(&self) -> Vec<LinkEvent> {
        self.0.try_iter().collect()
    }

    /// Panics if any event arrives within `window`.
    pub fn assert_quiet(&self, window: Duration) {
        if let Ok(event) = self.0.recv_timeout(window) {
            panic!("expected no more link events, got {}", summary(&[event]));
        }
    }
}

/// Events with long lines shortened, so a failing 16 MiB case doesn't print 16 MiB.
fn summary(events: &[LinkEvent]) -> String {
    let short: Vec<String> = events
        .iter()
        .map(|e| match e {
            LinkEvent::Line { conn, line } if line.len() > 80 => {
                format!("Line {{ {conn}, {} bytes }}", line.len())
            }
            LinkEvent::Line { conn, line } => {
                format!("Line {{ {conn}, {:?} }}", String::from_utf8_lossy(line))
            }
            other => format!("{other:?}"),
        })
        .collect();
    format!("[{}]", short.join(", "))
}

/// A connected client end, with its events and both ids of its conn.
pub struct Client<C> {
    pub link: C,
    /// The conn as the client knows it.
    pub conn: ConnId,
    /// The same conn as the host knows it.
    pub host_conn: ConnId,
    pub events: Events,
}

impl<C: Link> Client<C> {
    pub fn send(&mut self, line: impl AsRef<[u8]>) {
        self.link.send(self.conn, line.as_ref()).unwrap();
    }
}

/// The host's sink. Besides forwarding, it counts `Line`s for conns [`LinkHarness::close`] has already returned
/// for, which the port promises never happen.
struct HostSink {
    inner: ChannelSink,
    closed: Mutex<HashSet<ConnId>>,
    late_lines: AtomicUsize,
}

impl LinkEventSink for HostSink {
    fn send(&self, event: LinkEvent) {
        if let LinkEvent::Line { conn, .. } = &event {
            let closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
            if closed.contains(conn) {
                self.late_lines.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.inner.send(event);
    }
}

/// A listening host end plus its events, and the fixture to connect clients with.
pub struct LinkHarness<F: LinkFixture> {
    fixture: F,
    pub host: F::Host,
    pub host_events: Events,
    host_sink: Arc<HostSink>,
}

impl<F: LinkFixture> LinkHarness<F> {
    pub fn new(mut fixture: F) -> Self {
        let (tx, rx) = mpsc::channel();
        let host_sink = Arc::new(HostSink {
            inner: ChannelSink(tx),
            closed: Mutex::default(),
            late_lines: AtomicUsize::new(0),
        });
        let host = fixture.listen(host_sink.clone());
        Self {
            fixture,
            host,
            host_events: Events(rx),
            host_sink,
        }
    }

    /// Closes `conn` on the host and records that `close` returned, so later `Line`s for it are counted.
    pub fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.host.close(conn)?;
        let mut closed = self
            .host_sink
            .closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        closed.insert(conn);
        Ok(())
    }

    /// How many `Line`s the host got for a conn after [`close`](Self::close) returned for it.
    pub fn late_lines(&self) -> usize {
        self.host_sink.late_lines.load(Ordering::SeqCst)
    }

    /// Connects a client and waits for the host to see it. Connect clients before sending traffic: panics if
    /// the host reports anything but `Connected` first.
    pub fn connect(&mut self) -> Client<F::Client> {
        let (sink, events) = Events::channel();
        let (link, conn) = self.fixture.connect(sink);
        let host_conn = self.next_connected();
        Client {
            link,
            conn,
            host_conn,
            events,
        }
    }

    /// Connects a peer that never reads. Returns it and its conn on the host.
    pub fn stalled_peer(&mut self) -> (F::Stalled, ConnId) {
        let peer = self.fixture.stalled_peer();
        (peer, self.next_connected())
    }

    /// Connects a peer that never reads and drops it at once, before the host has necessarily seen it. Returns
    /// its conn on the host.
    pub fn short_lived_peer(&mut self) -> ConnId {
        drop(self.fixture.stalled_peer());
        self.next_connected()
    }

    pub fn send(&mut self, conn: ConnId, line: impl AsRef<[u8]>) {
        self.host.send(conn, line.as_ref()).unwrap();
    }

    pub fn fixture(&self) -> &F {
        &self.fixture
    }

    fn next_connected(&self) -> ConnId {
        let events = self.host_events.wait_for(|_| true, TIMEOUT);
        match events.as_slice() {
            [LinkEvent::Connected { conn }] => *conn,
            other => panic!("expected Connected on the host, got {}", summary(other)),
        }
    }
}

fn line(tag: &str, i: usize) -> Vec<u8> {
    format!(r#"{{"from":"{tag}","n":{i}}}"#).into_bytes()
}

fn lines(tag: &str, n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| line(tag, i)).collect()
}

pub fn lines_reach_the_host_in_order<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    for l in lines("client", 100) {
        c.send(l);
    }
    assert_eq!(
        h.host_events.lines_of(c.host_conn, 100),
        lines("client", 100)
    );
}

pub fn lines_reach_the_client_in_order<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    for l in lines("host", 100) {
        h.send(c.host_conn, l);
    }
    assert_eq!(c.events.lines_of(c.conn, 100), lines("host", 100));
}

pub fn ten_thousand_lines_arrive_in_order_both_ways<F: LinkFixture>(mut h: LinkHarness<F>) {
    const N: usize = 10_000;
    let mut c = h.connect();
    for i in 0..N {
        h.send(c.host_conn, line("host", i));
        c.send(line("client", i));
    }
    assert_eq!(h.host_events.lines_of(c.host_conn, N), lines("client", N));
    assert_eq!(c.events.lines_of(c.conn, N), lines("host", N));
}

pub fn several_clients_at_once_each_get_their_own_lines<F: LinkFixture>(mut h: LinkHarness<F>) {
    const CLIENTS: usize = 4;
    const N: usize = 50;
    let mut clients: Vec<_> = (0..CLIENTS).map(|_| h.connect()).collect();
    let mut host_conns: Vec<ConnId> = clients.iter().map(|c| c.host_conn).collect();
    host_conns.sort();
    host_conns.dedup();
    assert_eq!(host_conns.len(), CLIENTS, "each client is its own conn");

    let tag = |k: usize| format!("client{k}");
    for i in 0..N {
        for (k, c) in clients.iter_mut().enumerate() {
            c.send(line(&tag(k), i));
        }
    }
    let expected: Vec<_> = clients.iter().map(|c| (c.host_conn, N)).collect();
    let got = h.host_events.lines(&expected);
    for (k, c) in clients.iter().enumerate() {
        assert_eq!(got[&c.host_conn], lines(&tag(k), N), "{}", tag(k));
    }

    for (k, c) in clients.iter().enumerate() {
        h.send(c.host_conn, format!("to-{k}"));
    }
    for (k, c) in clients.iter().enumerate() {
        assert_eq!(
            c.events.lines_of(c.conn, 1),
            [format!("to-{k}").into_bytes()]
        );
        c.events.assert_quiet(Duration::ZERO);
    }
}

pub fn host_close_disconnects_both_ends<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    h.host.close(c.host_conn).unwrap();
    assert_eq!(
        h.host_events.disconnect_of(c.host_conn).1,
        DisconnectReason::Closed
    );
    assert_eq!(c.events.disconnect_of(c.conn).1, DisconnectReason::PeerGone);
}

pub fn client_close_disconnects_both_ends<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    c.link.close(c.conn).unwrap();
    assert_eq!(c.events.disconnect_of(c.conn).1, DisconnectReason::Closed);
    assert_eq!(
        h.host_events.disconnect_of(c.host_conn).1,
        DisconnectReason::PeerGone
    );
}

pub fn lines_queued_before_host_close_are_delivered<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    for l in lines("host", 1000) {
        h.send(c.host_conn, l);
    }
    h.host.close(c.host_conn).unwrap();
    let (got, reason) = c.events.disconnect_of(c.conn);
    assert_eq!(got, lines("host", 1000));
    assert_eq!(reason, DisconnectReason::PeerGone);
}

pub fn lines_queued_before_client_close_are_delivered<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    for l in lines("client", 1000) {
        c.send(l);
    }
    c.link.close(c.conn).unwrap();
    let (got, reason) = h.host_events.disconnect_of(c.host_conn);
    assert_eq!(got, lines("client", 1000));
    assert_eq!(reason, DisconnectReason::PeerGone);
}

pub fn disconnect_is_sent_once_and_last<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    c.send("before close");
    h.host.close(c.host_conn).unwrap();
    h.host_events.disconnect_of(c.host_conn);
    c.events.disconnect_of(c.conn);
    h.host_events.assert_quiet(QUIET);
    c.events.assert_quiet(Duration::ZERO);
}

pub fn closed_conn_is_unknown<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    h.host.close(c.host_conn).unwrap();
    assert_eq!(h.host.send(c.host_conn, b"x"), Err(LinkError::UnknownConn));
    assert_eq!(h.host.close(c.host_conn), Err(LinkError::UnknownConn));

    c.events.disconnect_of(c.conn);
    assert_eq!(c.link.send(c.conn, b"x"), Err(LinkError::UnknownConn));
    assert_eq!(c.link.close(c.conn), Err(LinkError::UnknownConn));
}

pub fn unknown_conn_is_an_error<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    let nobody = ConnId::new(u64::MAX);
    assert_eq!(h.host.send(nobody, b"x"), Err(LinkError::UnknownConn));
    assert_eq!(h.host.close(nobody), Err(LinkError::UnknownConn));
    assert_eq!(c.link.send(nobody, b"x"), Err(LinkError::UnknownConn));
    assert_eq!(c.link.close(nobody), Err(LinkError::UnknownConn));
}

pub fn line_containing_a_newline_is_invalid<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    assert_eq!(c.link.send(c.conn, b"{}\n{}"), Err(LinkError::InvalidLine));
    assert_eq!(
        h.host.send(c.host_conn, b"{}\n"),
        Err(LinkError::InvalidLine)
    );
    // Nothing of the rejected lines is sent, and the conn still works.
    c.send("ok");
    h.send(c.host_conn, "ok");
    assert_eq!(h.host_events.lines_of(c.host_conn, 1), [b"ok".to_vec()]);
    assert_eq!(c.events.lines_of(c.conn, 1), [b"ok".to_vec()]);
}

pub fn dropping_a_client_is_peer_gone_on_the_host<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    drop(c.link);
    assert_eq!(
        h.host_events.disconnect_of(c.host_conn).1,
        DisconnectReason::PeerGone
    );
}

pub fn dropping_the_host_is_peer_gone_on_every_client<F: LinkFixture>(mut h: LinkHarness<F>) {
    let a = h.connect();
    let b = h.connect();
    drop(h.host);
    assert_eq!(a.events.disconnect_of(a.conn).1, DisconnectReason::PeerGone);
    assert_eq!(b.events.disconnect_of(b.conn).1, DisconnectReason::PeerGone);
}

pub fn peer_that_dies_abruptly_is_peer_gone<F: LinkFixture>(mut h: LinkHarness<F>) {
    let (peer, conn) = h.stalled_peer();
    h.send(conn, "you there?");
    drop(peer);
    assert_eq!(
        h.host_events.disconnect_of(conn).1,
        DisconnectReason::PeerGone
    );
    assert_eq!(h.host.send(conn, b"x"), Err(LinkError::UnknownConn));
}

/// A client that is gone before the host gets to it is still announced, so every `Disconnected` has a
/// `Connected`. Many at once, to catch the host setting up a conn whose peer already left.
pub fn peer_that_dies_right_after_connecting_is_announced_then_gone<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    for _ in 0..20 {
        let conn = h.short_lived_peer();
        assert_eq!(
            h.host_events.disconnect_of(conn).1,
            DisconnectReason::PeerGone
        );
    }
}

/// `send` must not block on a peer that never reads. The sends run on another thread: if one blocked, the
/// case fails on the liveness timeout instead of hanging. The work is counted, not timed: the conn must be dropped
/// well before the bytes sent reach a few times the queue bound.
pub fn peer_that_never_reads_is_disconnected_as_slow_consumer<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let c = h.connect();
    let (_peer, conn) = h.stalled_peer();
    let limit = 4 * MAX_QUEUED;
    let mut host = h.host;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let chunk = vec![b'x'; 64 * 1024];
        let mut sent = 0;
        let mut dropped = false;
        while sent < limit {
            match host.send(conn, &chunk) {
                Ok(()) => sent += chunk.len(),
                Err(LinkError::UnknownConn) => {
                    dropped = true;
                    break;
                }
                Err(e) => panic!("send failed: {e}"),
            }
        }
        let _ = tx.send((host, sent, dropped));
    });
    let (mut host, sent, dropped) = rx
        .recv_timeout(TIMEOUT)
        .unwrap_or_else(|_| panic!("send blocked on a peer that never reads"));
    assert!(dropped, "still connected after {sent} bytes queued");
    assert_eq!(
        h.host_events.disconnect_of(conn).1,
        DisconnectReason::SlowConsumer
    );
    assert_eq!(host.send(conn, b"x"), Err(LinkError::UnknownConn));

    // Other conns are unaffected.
    host.send(c.host_conn, b"still here").unwrap();
    assert_eq!(c.events.lines_of(c.conn, 1), [b"still here".to_vec()]);
}

/// Closing a conn while its peer is still streaming: once `close` returns, no more of its `Line`s are emitted.
pub fn no_lines_arrive_after_close_returns<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    for l in lines("client", 5000) {
        c.send(l);
    }
    h.host_events.lines_of(c.host_conn, 1);
    h.close(c.host_conn).unwrap();
    assert_eq!(
        h.host_events.disconnect_of(c.host_conn).1,
        DisconnectReason::Closed
    );
    assert_eq!(h.late_lines(), 0, "lines emitted after close returned");
}

pub fn line_over_the_limit_is_a_protocol_error<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    c.send("ok");
    // Wait for it so the long line goes into an empty queue.
    assert_eq!(h.host_events.lines_of(c.host_conn, 1), [b"ok".to_vec()]);
    c.send(vec![b'x'; MAX_LINE + 1]);
    let (got, reason) = h.host_events.disconnect_of(c.host_conn);
    assert!(got.is_empty(), "no part of the long line is delivered");
    assert!(
        matches!(reason, DisconnectReason::Protocol(_)),
        "got {reason:?}"
    );
    assert_eq!(c.events.disconnect_of(c.conn).1, DisconnectReason::PeerGone);
}

pub fn other_conns_survive_a_disconnect<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut gone = h.connect();
    let mut stays = h.connect();
    gone.link.close(gone.conn).unwrap();
    h.host_events.disconnect_of(gone.host_conn);

    stays.send("ping");
    assert_eq!(
        h.host_events.lines_of(stays.host_conn, 1),
        [b"ping".to_vec()]
    );
    h.send(stays.host_conn, "pong");
    assert_eq!(stays.events.lines_of(stays.conn, 1), [b"pong".to_vec()]);
}

/// Spins until `done` holds. Panics after [`TIMEOUT`]: a liveness bound, not a timing assertion.
fn spin_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::yield_now();
    }
}

const CHUNK: usize = 64 * 1024;
/// Well over what a socket and a writer's batch can hold, well under [`MAX_QUEUED`].
const BACKLOG: usize = 32 * CHUNK;

pub fn queued_bytes_rise_while_the_peer_does_not_read<F: LinkFixture>(mut h: LinkHarness<F>) {
    let (_peer, conn) = h.stalled_peer();
    assert_eq!(h.host.queued_bytes(conn), Some(0));
    let chunk = vec![b'x'; CHUNK];
    for _ in 0..BACKLOG / CHUNK {
        h.send(conn, &chunk);
    }
    // Some of it may sit in the socket or the writer's batch, but most has nowhere to go.
    spin_until("most of the backlog is queued", || {
        h.host.queued_bytes(conn).is_some_and(|q| q >= BACKLOG / 2)
    });
    let queued = h.host.queued_bytes(conn).unwrap();
    assert!(queued <= BACKLOG + BACKLOG / CHUNK, "{queued} bytes queued");
}

pub fn queued_bytes_fall_to_zero_once_the_peer_has_read_everything<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let c = h.connect();
    let chunk = vec![b'x'; CHUNK];
    for _ in 0..BACKLOG / CHUNK {
        h.send(c.host_conn, &chunk);
    }
    assert!(h.host.queued_bytes(c.host_conn).is_some());
    c.events.lines_of(c.conn, BACKLOG / CHUNK);
    spin_until("the queue is empty", || {
        h.host.queued_bytes(c.host_conn) == Some(0)
    });
}

pub fn queued_bytes_of_an_unknown_or_closed_conn_is_none<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    assert_eq!(h.host.queued_bytes(ConnId::new(u64::MAX)), None);
    assert_eq!(c.link.queued_bytes(ConnId::new(u64::MAX)), None);
    assert_eq!(c.link.queued_bytes(c.conn), Some(0));
    h.host.close(c.host_conn).unwrap();
    assert_eq!(h.host.queued_bytes(c.host_conn), None);
}

pub fn flush_closed_with_nothing_closed_is_flushed<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    h.send(c.host_conn, "open, not closed");
    assert_eq!(h.host.flush_closed(Duration::ZERO), FlushOutcome::Flushed);
    assert_eq!(c.link.flush_closed(Duration::ZERO), FlushOutcome::Flushed);
}

/// A backlog bigger than a socket holds, so the close still has delivering to do when it returns.
pub fn closed_conn_is_flushed_once_its_peer_has_read_everything<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let c = h.connect();
    let chunk = vec![b'x'; CHUNK];
    for _ in 0..BACKLOG / CHUNK {
        h.send(c.host_conn, &chunk);
    }
    h.host.close(c.host_conn).unwrap();
    assert_eq!(h.host.flush_closed(TIMEOUT), FlushOutcome::Flushed);
    // Delivered already, not just on its way: all that can still be in flight is what one socket buffer and one
    // read hold, a few chunks at most.
    let arrived = c.events.ready();
    let lines = |events: &[LinkEvent]| {
        events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Line { .. }))
            .count()
    };
    assert!(
        lines(&arrived) >= BACKLOG / CHUNK / 2,
        "{} of {} lines had arrived when flush_closed returned",
        lines(&arrived),
        BACKLOG / CHUNK
    );
    let mut rest = Vec::new();
    if !arrived
        .iter()
        .any(|e| matches!(e, LinkEvent::Disconnected { .. }))
    {
        rest = c
            .events
            .wait_for(|e| matches!(e, LinkEvent::Disconnected { .. }), TIMEOUT);
    }
    let all: Vec<LinkEvent> = arrived.into_iter().chain(rest).collect();
    assert_eq!(lines(&all), BACKLOG / CHUNK);
    assert_eq!(
        all.last(),
        Some(&LinkEvent::Disconnected {
            conn: c.conn,
            reason: DisconnectReason::PeerGone
        })
    );
}

/// Queues a backlog for a peer that never reads, until it is stuck: the peer's buffers full, most of it queued.
fn stuck_backlog<F: LinkFixture>(h: &mut LinkHarness<F>, conn: ConnId) {
    let chunk = vec![b'x'; CHUNK];
    for _ in 0..BACKLOG / CHUNK {
        h.send(conn, &chunk);
    }
    spin_until("most of the backlog is queued", || {
        h.host.queued_bytes(conn).is_some_and(|q| q >= BACKLOG / 2)
    });
}

/// The wait is bounded: a closed conn whose peer never reads can't hold `flush_closed` past its bound, and the
/// outcome says the delivery isn't done.
pub fn flush_closed_times_out_while_a_closed_peer_does_not_read<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let (_peer, conn) = h.stalled_peer();
    stuck_backlog(&mut h, conn);
    h.host.close(conn).unwrap();
    assert_eq!(h.host.flush_closed(Duration::ZERO), FlushOutcome::TimedOut);
    // A deadline still ahead is waited for, then given up on. However long this thread was held up since the close,
    // the outcome is the same: past `CLOSE_FLUSH` a link may have given up on the peer, which is no flush either
    // (`a_give_up_is_reported_once_and_holds_no_later_flush`).
    assert_eq!(h.host.flush_closed(QUIET), FlushOutcome::TimedOut);
}

/// A link gives up delivering to a closed conn whose peer hasn't read for [`CLOSE_FLUSH`]. What it gave up on was never
/// delivered, so the next `flush_closed` is `TimedOut`; then the give-up is forgotten, and doesn't hold a later
/// `flush_closed` for its bound. The first bound is well past `CLOSE_FLUSH`, so the link gives up while it waits. The
/// last bound is long, and returning well within it is a liveness bound, not a measurement.
pub fn a_give_up_is_reported_once_and_holds_no_later_flush<F: LinkFixture>(mut h: LinkHarness<F>) {
    let (_peer, conn) = h.stalled_peer();
    stuck_backlog(&mut h, conn);
    h.host.close(conn).unwrap();
    assert_eq!(h.host.flush_closed(2 * CLOSE_FLUSH), FlushOutcome::TimedOut);
    let long = 6 * TIMEOUT;
    let started = Instant::now();
    assert_eq!(h.host.flush_closed(long), FlushOutcome::Flushed);
    assert!(
        started.elapsed() < long / 2,
        "a conn given up on held flush_closed for {:?}",
        started.elapsed()
    );
}

pub fn closed_conn_whose_peer_goes_away_is_flushed<F: LinkFixture>(mut h: LinkHarness<F>) {
    let (peer, conn) = h.stalled_peer();
    stuck_backlog(&mut h, conn);
    h.host.close(conn).unwrap();
    drop(peer);
    assert_eq!(h.host.flush_closed(TIMEOUT), FlushOutcome::Flushed);
}

pub fn each_end_names_the_machine_the_other_is<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    assert_eq!(
        h.host.peer(c.host_conn),
        Ok(h.fixture().machine_of(&c.link))
    );
    assert_eq!(c.link.peer(c.conn), Ok(h.fixture().host_machine()));
}

/// An unknown conn is an error, never "local": a check that lets only local conns through must not let a conn that
/// just went through.
pub fn the_peer_of_an_unknown_or_gone_conn_is_an_error_not_local<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let c = h.connect();
    let gone = h.connect();
    assert_eq!(
        h.host.peer(ConnId::new(u64::MAX)),
        Err(LinkError::UnknownConn)
    );
    h.host.close(c.host_conn).unwrap();
    assert_eq!(h.host.peer(c.host_conn), Err(LinkError::UnknownConn));
    drop(gone.link);
    h.host_events.disconnect_of(gone.host_conn);
    assert_eq!(h.host.peer(gone.host_conn), Err(LinkError::UnknownConn));
}

/// Refused once the client's machine is known and before anything else: the host never announces it, and the client's
/// conn ends saying why.
pub fn a_machine_the_host_does_not_trust_is_refused_and_never_announced<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let (sink, events) = Events::channel();
    let Some((mut link, conn)) = h.fixture.untrusted(sink) else {
        return;
    };
    // Sent before the refusal arrives, or after: either way it goes nowhere.
    let _ = link.send(conn, b"let me in");
    assert_eq!(
        events.disconnect_of(conn),
        (vec![], DisconnectReason::Refused)
    );
    h.host_events.assert_quiet(QUIET);
}

pub fn a_paused_conn_emits_no_lines_until_resumed_then_all_in_order<F: LinkFixture>(
    mut h: LinkHarness<F>,
) {
    let mut c = h.connect();
    h.host.pause_reading(c.host_conn, true).unwrap();
    for l in lines("client", 100) {
        c.send(l);
    }
    h.host_events.assert_quiet(QUIET);
    h.host.pause_reading(c.host_conn, false).unwrap();
    assert_eq!(
        h.host_events.lines_of(c.host_conn, 100),
        lines("client", 100)
    );
}

/// The back-pressure a paused conn is for: what its peer sends piles up in the peer's own queue, and drains once the
/// conn is resumed.
pub fn the_peer_of_a_paused_conn_queues_what_it_sends<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    h.host.pause_reading(c.host_conn, true).unwrap();
    let chunk = vec![b'x'; CHUNK];
    for _ in 0..BACKLOG / CHUNK {
        c.send(&chunk);
    }
    spin_until("most of the backlog is queued at the client", || {
        c.link
            .queued_bytes(c.conn)
            .is_some_and(|q| q >= BACKLOG / 2)
    });
    h.host.pause_reading(c.host_conn, false).unwrap();
    h.host_events.lines_of(c.host_conn, BACKLOG / CHUNK);
    spin_until("the client's queue is empty", || {
        c.link.queued_bytes(c.conn) == Some(0)
    });
}

pub fn a_paused_conn_still_closes<F: LinkFixture>(mut h: LinkHarness<F>) {
    let mut c = h.connect();
    h.host.pause_reading(c.host_conn, true).unwrap();
    c.send("unread");
    h.host.close(c.host_conn).unwrap();
    assert_eq!(
        h.host_events.disconnect_of(c.host_conn),
        (vec![], DisconnectReason::Closed)
    );
    assert_eq!(c.events.disconnect_of(c.conn).1, DisconnectReason::PeerGone);
}

pub fn pausing_an_unknown_conn_is_an_error<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    let nobody = ConnId::new(u64::MAX);
    assert_eq!(
        h.host.pause_reading(nobody, true),
        Err(LinkError::UnknownConn)
    );
    h.host.close(c.host_conn).unwrap();
    assert_eq!(
        h.host.pause_reading(c.host_conn, true),
        Err(LinkError::UnknownConn)
    );
}

/// Counted as delivered: every byte of every line the peer read, its framing at most one byte a line.
pub fn drained_bytes_rise_as_the_peer_reads<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    assert_eq!(h.host.drained_bytes(c.host_conn), Some(0));
    let chunk = vec![b'x'; CHUNK];
    let n = BACKLOG / CHUNK;
    for _ in 0..n {
        h.send(c.host_conn, &chunk);
    }
    c.events.lines_of(c.conn, n);
    spin_until("everything read is counted", || {
        h.host
            .drained_bytes(c.host_conn)
            .is_some_and(|d| d >= BACKLOG as u64)
    });
    let drained = h.host.drained_bytes(c.host_conn).unwrap();
    assert!(drained <= (BACKLOG + n) as u64, "{drained} bytes drained");
}

pub fn drained_bytes_of_an_unknown_or_closed_conn_is_none<F: LinkFixture>(mut h: LinkHarness<F>) {
    let c = h.connect();
    assert_eq!(h.host.drained_bytes(ConnId::new(u64::MAX)), None);
    assert_eq!(c.link.drained_bytes(c.conn), Some(0));
    h.host.close(c.host_conn).unwrap();
    assert_eq!(h.host.drained_bytes(c.host_conn), None);
}

/// A link that reaches other machines opens the conn and ends it as `PeerGone`; one that reaches none (a local socket,
/// whose host has no machine) says so.
pub fn opening_a_conn_to_a_machine_nobody_hosts<F: LinkFixture>(mut h: LinkHarness<F>) {
    let nobody = h.fixture().nobody();
    match h.fixture().dials() {
        false => assert_eq!(h.host.open(&nobody), Err(LinkError::NoPeers)),
        true => {
            let conn = h.host.open(&nobody).unwrap();
            assert_eq!(
                h.host_events.disconnect_of(conn),
                (vec![], DisconnectReason::PeerGone)
            );
        }
    }
}
