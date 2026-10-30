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

use shepherd_core::ports::{
    ConnId, DisconnectReason, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED,
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
