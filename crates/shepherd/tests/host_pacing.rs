//! Host runtime scenarios about load (decision 0013 and the loop's deadlines), on all-fake ports.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shepherd::host::{
    CATCH_UP_BATCH, CATCH_UP_TICK, MAX_CATCH_UP_TICK, RuntimeEvent, SHUTDOWN_GRACE, STALL_TIMEOUT,
};
use shepherd_core::acpp::{Envelope, Seq};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    EventLog, LinkEvent, LinkEventSink, LogId, MAX_OUTPUT_CHUNK, MAX_QUEUED, OUTPUT_CREDIT,
    PtyError, PtyEvent, PtyEventSink,
};
use shepherd_core::wire::{ClientRequest, HostMessage, SessionRename};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::pty::ProgramInput;
use support::*;

/// A previous host's log for session `s_a`: created, then renamed until it holds more than `bytes` of names.
fn big_history(bytes: usize) -> FakeEventLog {
    let name = "n".repeat(8 * 1024);
    let mut prev = HostDriver::new();
    prev.new_session("s_a");
    for i in 0..bytes / name.len() + 1 {
        prev.rename("s_a", &format!("{i}-{name}"));
    }
    FakeEventLog::new().with_envelopes(LogId::Session(SessionId::new("s_a")), prev.log().to_vec())
}

fn is_response(m: &HostMessage, id: &shepherd_core::acpp::RequestId) -> bool {
    matches!(m, HostMessage::Response(r) if &r.id == id)
}

fn seqs_of(envelopes: &[Envelope], session: &str) -> Vec<u64> {
    envelopes
        .iter()
        .filter(|e| e.session().map(SessionId::as_str) == Some(session))
        .map(|e| e.seq().get())
        .collect()
}

#[test]
fn a_history_bigger_than_the_link_queue_reaches_a_slow_client_exactly_once_then_live() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let a = SessionId::new("s_a");
    let mut slow = h.connect_paced();
    let mut other = h.connect();
    let req = slow.subscribe_all();
    h.step();

    let mut msgs = Vec::new();
    let mut rounds = 0;
    let mut appended_during_catch_up = false;
    while !msgs.iter().any(|m| is_response(m, &req)) {
        rounds += 1;
        assert!(rounds < 10_000, "catch-up never finished");
        assert!(!slow.is_disconnected(), "dropped after {rounds} rounds");
        slow.read(256 * 1024);
        msgs.extend(slow.take());
        if !appended_during_catch_up && entries(&msgs).len() > 10 {
            // Entries for a log that is still catching up, and a brand-new log.
            other.request(ClientRequest::SessionRename(SessionRename {
                session: a.clone(),
                name: "during".into(),
            }));
            other.new_session();
            appended_during_catch_up = true;
        }
        h.clock().advance(CATCH_UP_TICK);
        h.step();
    }
    assert!(rounds > 4, "the catch-up was not paced ({rounds} rounds)");
    let ack = msgs.iter().position(|m| is_response(m, &req)).unwrap();

    // Live after the ack.
    other.request(ClientRequest::SessionRename(SessionRename {
        session: a.clone(),
        name: "after".into(),
    }));
    h.step();
    slow.read(usize::MAX);
    msgs.extend(slow.take());
    assert!(!slow.is_disconnected());

    let all = entries(&msgs);
    let total = h
        .log()
        .read(LogId::Session(a.clone()), shepherd_core::acpp::Seq::ZERO)
        .unwrap()
        .len() as u64;
    assert_eq!(seqs_of(&all, "s_a"), (1..=total).collect::<Vec<_>>());
    assert_eq!(seqs_of(&all, "s_t1"), [1]);
    let after_ack = entries(&msgs[ack..]);
    assert_eq!(seqs_of(&after_ack, "s_a"), [total]);
}

#[test]
fn pty_output_reaches_a_subscriber_only_after_its_catch_up() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED / 2))
        .start();
    let (mut typist, id) = {
        let mut c = h.connect();
        let req = c.new_session();
        h.step();
        let id = new_session_id(&c.take(), &req);
        (c, id)
    };
    let mut slow = h.connect_paced();
    let req = slow.subscribe_all();
    h.step();
    typist.input(&id, b"early");
    h.step();

    let mut msgs = Vec::new();
    while !msgs.iter().any(|m| is_response(m, &req)) {
        slow.read(256 * 1024);
        msgs.extend(slow.take());
        h.clock().advance(CATCH_UP_TICK);
        h.step();
    }
    let ack = msgs.iter().position(|m| is_response(m, &req)).unwrap();
    // Only the snapshot, which already shows "early", comes before the ack; no raw output during catch-up.
    let before = output(&msgs[..ack], &id);
    let mut screen = shepherd_term::Screen::new(80, 24);
    screen.feed(&before);
    let row: String = screen.row(0).map(|c| c.ch).collect();
    assert_eq!(row.trim_end(), "early");
    assert!(!String::from_utf8_lossy(&before).starts_with("early"));

    typist.input(&id, b"!");
    h.step();
    slow.read(usize::MAX);
    assert_eq!(output(&slow.take(), &id), b"!");
}

/// Runs the loop deadline by deadline, moving the clock to each one, until `done`. Returns the waits between
/// them. Panics after 10,000 deadlines.
fn run_deadlines(
    h: &mut HostHarness,
    mut done: impl FnMut(&mut HostHarness) -> bool,
) -> Vec<Duration> {
    let clock = h.clock();
    let mut waits = Vec::new();
    while !done(h) {
        assert!(
            waits.len() < 10_000,
            "never done after {} deadlines",
            waits.len()
        );
        let due = h.runtime().next_deadline().expect("a pending deadline");
        let wait = due.saturating_duration_since(clock.now());
        clock.advance(wait);
        waits.push(wait);
        h.step();
    }
    waits
}

#[test]
fn a_client_that_stops_reading_mid_catch_up_is_closed_after_the_stall_window() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let a = SessionId::new("s_a");
    let mut other = h.connect();
    let other_req = other.subscribe_all();
    h.step();
    assert!(other.take().iter().any(|m| is_response(m, &other_req)));

    let mut stuck = h.connect_paced();
    stuck.subscribe_all();
    h.step();
    let waits = run_deadlines(&mut h, |_| stuck.is_disconnected());

    // Backs off from the first tick, doubling, to the cap.
    let ms = |n| Duration::from_millis(n);
    assert_eq!(
        waits[..8],
        [
            ms(5),
            ms(10),
            ms(20),
            ms(40),
            ms(80),
            ms(160),
            ms(320),
            ms(500)
        ]
    );
    assert!(
        waits[8..].iter().all(|w| *w == MAX_CATCH_UP_TICK),
        "{waits:?}"
    );
    // Closed once the window passed, not before.
    let stalled: Duration = waits.iter().sum();
    assert!(stalled >= STALL_TIMEOUT, "closed after {stalled:?}");
    assert!(
        stalled < STALL_TIMEOUT + MAX_CATCH_UP_TICK,
        "closed after {stalled:?}"
    );
    assert_eq!(h.runtime().next_deadline(), None);

    other.request(ClientRequest::SessionRename(SessionRename {
        session: a.clone(),
        name: "still live".into(),
    }));
    h.step();
    assert_eq!(entries(&other.take()).len(), 1);
}

#[test]
fn a_slow_client_that_keeps_draining_is_never_closed() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let mut slow = h.connect_paced();
    let req = slow.subscribe_all();
    h.step();
    let mut msgs = Vec::new();
    let mut checks = 0;
    // A trickle: 64 KiB every eighth check, so the whole catch-up takes far longer than the stall window, while
    // the queue keeps shrinking well within it.
    let waits = run_deadlines(&mut h, |h| {
        checks += 1;
        if h.runtime().next_deadline().is_none() {
            // Catch-up is done; read the rest, the ack included.
            slow.read(usize::MAX);
        } else if checks % 8 == 0 {
            slow.read(64 * 1024);
        }
        msgs.extend(slow.take());
        assert!(!slow.is_disconnected());
        msgs.iter().any(|m| is_response(m, &req))
    });
    assert!(
        waits.iter().sum::<Duration>() > STALL_TIMEOUT,
        "{:?}",
        waits.iter().sum::<Duration>()
    );
}

#[test]
fn a_blocked_client_whose_queue_keeps_shrinking_is_never_closed() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let mut slow = h.connect_paced();
    slow.subscribe_all();
    h.step();
    // Live entries for a session created after the subscribe pile up on top of the blocked catch-up, so the
    // queue stays above the low-water mark while the client drains it one line at a time.
    let mut other = h.connect();
    let req = other.new_session();
    h.step();
    let session = new_session_id(&other.take(), &req);
    let name = "m".repeat(8 * 1024);
    for i in 0..200 {
        other.request(ClientRequest::SessionRename(SessionRename {
            session: session.clone(),
            name: format!("{i}-{name}"),
        }));
    }
    h.step();

    // 100 lines over 100 s: far past the stall window, but the queue shrinks at every check.
    for _ in 0..100 {
        slow.read(1);
        h.clock().advance(Duration::from_secs(1));
        h.step();
        assert!(!slow.is_disconnected());
    }
}

/// A [`FakeEventLog`] that counts the entries its reads return (what a real log would parse), and the most any one
/// read returned (what a reader has to hold at once).
struct CountingLog {
    inner: FakeEventLog,
    parsed: Arc<AtomicUsize>,
    largest: Arc<AtomicUsize>,
}

impl CountingLog {
    fn count(&self, read: &[Envelope]) {
        self.parsed.fetch_add(read.len(), Ordering::SeqCst);
        self.largest.fetch_max(read.len(), Ordering::SeqCst);
    }
}

impl EventLog for CountingLog {
    fn append(
        &mut self,
        log: LogId,
        draft: shepherd_core::acpp::FrameDraft,
        stamp: shepherd_core::acpp::Stamp,
    ) -> Result<Envelope, shepherd_core::ports::LogError> {
        self.inner.append(log, draft, stamp)
    }

    fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, shepherd_core::ports::LogError> {
        let read = self.inner.read(log, from)?;
        self.count(&read);
        Ok(read)
    }

    fn read_batch(
        &self,
        log: LogId,
        from: Seq,
        max: usize,
    ) -> Result<Vec<Envelope>, shepherd_core::ports::LogError> {
        let read = self.inner.read_batch(log, from, max)?;
        self.count(&read);
        Ok(read)
    }

    fn logs(&self) -> Result<Vec<LogId>, shepherd_core::ports::LogError> {
        self.inner.logs()
    }
}

#[test]
fn a_paced_catch_up_reads_each_entry_about_once() {
    let parsed = Arc::new(AtomicUsize::new(0));
    let largest = Arc::new(AtomicUsize::new(0));
    let log = CountingLog {
        inner: big_history(MAX_QUEUED * 3 / 2),
        parsed: parsed.clone(),
        largest: largest.clone(),
    };
    let net = shepherd_testkit::fakes::link::FakeNetwork::new();
    let env = TestEnv::new();
    let clock = env.clock();
    let mut rt = runtime_with(&net, &[(SHELL.to_owned(), echo_shell())], log, env, |s| s);
    rt.run_until_idle();
    let total = rt
        .log()
        .inner
        .read(LogId::Session(SessionId::new("s_a")), Seq::ZERO)
        .unwrap()
        .len();
    parsed.store(0, Ordering::SeqCst);
    largest.store(0, Ordering::SeqCst);

    let mut slow = FakeClient::connect_with(&net, true);
    let req = slow.subscribe_all();
    rt.run_until_idle();
    let mut msgs = Vec::new();
    let mut rounds = 0;
    while !msgs.iter().any(|m| is_response(m, &req)) {
        rounds += 1;
        assert!(rounds < 10_000);
        slow.read(64 * 1024);
        msgs.extend(slow.take());
        if let Some(due) = rt.next_deadline() {
            clock.advance(due.saturating_duration_since(clock.now()));
        }
        rt.run_until_idle();
    }
    assert!(rounds > 100, "not paced: {rounds} rounds");
    assert_eq!(entries(&msgs).len(), total);
    let parsed = parsed.load(Ordering::SeqCst);
    assert!(
        parsed <= total + CATCH_UP_BATCH,
        "parsed {parsed} entries for {total}"
    );
    assert!(largest.load(Ordering::SeqCst) <= CATCH_UP_BATCH);
}

/// Passes link events through, except `Disconnected`, which is held back as if still in flight.
struct HoldDisconnects(Arc<dyn shepherd_core::ports::LinkEventSink>);

impl shepherd_core::ports::LinkEventSink for HoldDisconnects {
    fn send(&self, event: shepherd_core::ports::LinkEvent) {
        if !matches!(event, shepherd_core::ports::LinkEvent::Disconnected { .. }) {
            self.0.send(event);
        }
    }
}

#[test]
fn a_catching_up_conn_found_gone_leaves_no_deadline_before_its_disconnect_arrives() {
    let net = shepherd_testkit::fakes::link::FakeNetwork::new();
    let env = TestEnv::new();
    let clock = env.clock();
    let mut rt = runtime_with(
        &net,
        &[(SHELL.to_owned(), echo_shell())],
        big_history(MAX_QUEUED * 3 / 2),
        env,
        |sink| Arc::new(HoldDisconnects(sink)),
    );
    let mut slow = FakeClient::connect_with(&net, true);
    slow.subscribe_all();
    rt.run_until_idle();
    let due = rt.next_deadline().expect("a blocked catch-up");

    drop(slow);
    clock.advance(due.saturating_duration_since(clock.now()));
    rt.run_until_idle();
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn a_newer_subscribe_replaces_an_unfinished_one_and_the_old_request_gets_an_error() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let mut slow = h.connect_paced();
    let first = slow.subscribe_all();
    h.step();
    let second = slow.subscribe_all();
    h.step();

    let mut msgs = Vec::new();
    run_deadlines(&mut h, |_| {
        slow.read(usize::MAX);
        msgs.extend(slow.take());
        msgs.iter().any(|m| is_response(m, &second))
    });
    assert!(matches!(
        response(&msgs, &first),
        Err(shepherd_core::wire::WireError::InvalidRequest(_))
    ));
    assert!(response(&msgs, &second).is_ok());
    // The second catch-up is whole on its own: every entry from its first, in order, after the first's error.
    let error_at = msgs.iter().position(|m| is_response(m, &first)).unwrap();
    let total = h
        .log()
        .read(
            LogId::Session(SessionId::new("s_a")),
            shepherd_core::acpp::Seq::ZERO,
        )
        .unwrap()
        .len() as u64;
    assert_eq!(
        seqs_of(&entries(&msgs[error_at..]), "s_a"),
        (1..=total).collect::<Vec<_>>()
    );
}

#[test]
fn a_client_that_disconnects_mid_catch_up_is_forgotten() {
    let mut h = HostHarness::builder()
        .log(big_history(MAX_QUEUED * 3 / 2))
        .start();
    let mut gone = h.connect_paced();
    gone.subscribe_all();
    h.step();
    assert!(h.runtime().next_deadline().is_some());
    drop(gone);
    h.step();
    assert_eq!(h.runtime().next_deadline(), None);

    let mut later = h.connect();
    let req = later.subscribe_all();
    h.step();
    let msgs = later.take();
    assert!(response(&msgs, &req).is_ok());
    let total = h
        .log()
        .read(
            LogId::Session(SessionId::new("s_a")),
            shepherd_core::acpp::Seq::ZERO,
        )
        .unwrap()
        .len() as u64;
    assert_eq!(
        seqs_of(&entries(&msgs), "s_a"),
        (1..=total).collect::<Vec<_>>()
    );
}

// ---------- deadlines under load ----------

#[test]
fn a_sync_hold_is_released_even_while_another_session_floods_the_loop() {
    let stdin = Arc::new(Mutex::new(Vec::new()));
    let record = stdin.clone();
    let spawned = Arc::new(Mutex::new(0));
    // The first session stays quiet; the second asks for DA1 inside a synchronized update that never ends.
    let mut h = HostHarness::builder()
        .program(
            SHELL,
            Arc::new(move |input, p| match input {
                ProgramInput::Start => {
                    let mut n = spawned.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        p.print(b"\x1b[?2026h\x1b[c");
                    }
                }
                ProgramInput::Stdin(bytes) => record.lock().unwrap().extend_from_slice(bytes),
                ProgramInput::Resized => {}
            }),
        )
        .start();
    let mut client = h.connect();
    let first = client.new_session();
    client.new_session();
    h.step();
    let flooder = new_session_id(&client.take(), &first);
    assert!(stdin.lock().unwrap().is_empty());

    let tx = h.runtime().sender();
    for _ in 0..10_000 {
        tx.send(RuntimeEvent::Pty(PtyEvent::Output {
            session: flooder.clone(),
            bytes: b"y\r\n".to_vec(),
        }));
    }
    tx.send(RuntimeEvent::Shutdown);
    h.clock().advance(Duration::from_secs(5));
    h.runtime_mut().run();

    assert!(
        stdin.lock().unwrap().starts_with(b"\x1b[?"),
        "the hold was starved"
    );
}

#[test]
fn shutdown_jumps_ahead_of_a_flood_of_output() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.subscribe_all();
    let req = client.new_session();
    h.step();
    let id = new_session_id(&client.take(), &req);

    let tx = h.runtime().sender();
    for _ in 0..10_000 {
        tx.send(RuntimeEvent::Pty(PtyEvent::Output {
            session: id.clone(),
            bytes: b"y".to_vec(),
        }));
    }
    tx.send(RuntimeEvent::Shutdown);
    h.runtime_mut().run();

    assert!(h.runtime().is_finished());
    // None of the queued output was worked through: shutdown started first and dropped it.
    assert!(output(&client.take(), &id).is_empty());
    let log = h
        .log()
        .read(LogId::Session(id), shepherd_core::acpp::Seq::ZERO)
        .unwrap();
    assert!(log.iter().any(|e| matches!(
        e.frame(),
        shepherd_core::acpp::Frame::Shepherd(m)
            if matches!(m.body(), shepherd_core::acpp::ShepherdFrame::AgentExited(_))
    )));
}

#[test]
fn shutdown_stops_at_its_deadline_even_while_the_loop_is_flooded() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.subscribe_all();
    let req = client.new_session();
    h.step();
    let id = new_session_id(&client.take(), &req);

    // The kill fails, so shutdown waits for an exit that never comes, until its deadline.
    h.runtime_mut()
        .pty_mut()
        .fail_next(PtyError::Io("stuck".into()));
    h.send(RuntimeEvent::Shutdown);
    assert!(!h.runtime().is_finished());

    let tx = h.runtime().sender();
    for _ in 0..10_000 {
        tx.send(RuntimeEvent::Pty(PtyEvent::Output {
            session: id.clone(),
            bytes: b"y".to_vec(),
        }));
    }
    h.clock().advance(SHUTDOWN_GRACE + Duration::from_secs(1));
    h.runtime_mut().run();

    assert!(h.runtime().is_finished());
    // Stopped first thing, without working through the flood.
    assert!(output(&client.take(), &id).is_empty());
    assert!(client.is_disconnected());
}

// ---------- PTY backpressure ----------

/// Counts every event a port pushes into the loop's channel, and the PTY output bytes among them.
#[derive(Clone, Default)]
struct Pushed {
    events: Arc<AtomicUsize>,
    output: Arc<AtomicUsize>,
}

struct CountLink(Arc<dyn LinkEventSink>, Pushed);

impl LinkEventSink for CountLink {
    fn send(&self, event: LinkEvent) {
        self.1.events.fetch_add(1, Ordering::SeqCst);
        self.0.send(event);
    }
}

struct CountPty(Arc<dyn PtyEventSink>, Pushed);

impl PtyEventSink for CountPty {
    fn send(&self, event: PtyEvent) {
        self.1.events.fetch_add(1, Ordering::SeqCst);
        if let PtyEvent::Output { bytes, .. } = &event {
            self.1.output.fetch_add(bytes.len(), Ordering::SeqCst);
        }
        self.0.send(event);
    }
}

/// A host whose loop is stepped one event at a time, checking after each that the channel and the flooding
/// session's unacknowledged output stay bounded.
struct Flooded {
    rt: Runtime,
    client: FakeClient,
    msgs: Vec<HostMessage>,
    pushed: Pushed,
    handled: usize,
    flooder: SessionId,
}

impl Flooded {
    /// A flood holds at most a credit's worth of chunks in the channel (plus the one read past it), and each other
    /// event waits behind at most that many. The slack is for the client's own events.
    const BOUND: usize = OUTPUT_CREDIT / MAX_OUTPUT_CHUNK + 1 + 4;

    /// Handles one event at a time until `done`, which must happen within `trips` passes through a full queue
    /// ([`Self::BOUND`] events each).
    fn run_until(&mut self, trips: usize, mut done: impl FnMut(&[HostMessage]) -> bool) {
        let mut steps = 0;
        while !done(&self.msgs) {
            assert!(
                steps < trips * Self::BOUND,
                "starved: not done after {steps} events"
            );
            steps += 1;
            self.step();
        }
    }

    fn step(&mut self) {
        self.handled += self.rt.run_events(1);
        let queued = self.pushed.events.load(Ordering::SeqCst) - self.handled;
        assert!(queued <= Self::BOUND, "{queued} events queued");
        if let Some(unacked) = self.rt.pty().unacked(&self.flooder) {
            assert!(
                unacked < OUTPUT_CREDIT + MAX_OUTPUT_CHUNK,
                "{unacked} bytes unacked"
            );
        }
        self.msgs.extend(self.client.take());
    }

    fn answered(msgs: &[HostMessage], id: &shepherd_core::acpp::RequestId) -> bool {
        msgs.iter().any(|m| is_response(m, id))
    }
}

#[test]
fn a_flooding_session_is_paced_by_acks_and_starves_no_other_session_or_client() {
    // The first session floods forever. The second asks for DA1 whenever it is typed "query", and records what
    // the terminal answers.
    let replies = Arc::new(Mutex::new(Vec::new()));
    let record = replies.clone();
    let spawned = Arc::new(AtomicUsize::new(0));
    let program: Program = Arc::new(move |input, p| match input {
        ProgramInput::Start => {
            if spawned.fetch_add(1, Ordering::SeqCst) == 0 {
                p.flood("y\r\n");
            }
        }
        ProgramInput::Stdin(b"query") => p.print(b"\x1b[c"),
        ProgramInput::Stdin(bytes) => record.lock().unwrap().extend_from_slice(bytes),
        ProgramInput::Resized => {}
    });
    let pushed = Pushed::default();
    let net = shepherd_testkit::fakes::link::FakeNetwork::new();
    let (for_link, for_pty) = (pushed.clone(), pushed.clone());
    let rt = runtime_with_sinks(
        &net,
        &[(SHELL.to_owned(), program)],
        FakeEventLog::new(),
        TestEnv::new(),
        |sink| Arc::new(CountLink(sink, for_link)),
        |sink| Arc::new(CountPty(sink, for_pty)),
    );
    let client = FakeClient::connect_with(&net, false);
    let mut h = Flooded {
        rt,
        client,
        msgs: Vec::new(),
        pushed,
        handled: 0,
        flooder: SessionId::new("s_t1"),
    };

    let first = h.client.new_session();
    h.run_until(1, |m| Flooded::answered(m, &first));
    assert_eq!(new_session_id(&h.msgs, &first), h.flooder);

    // A client request is still handled while the flood goes on.
    let second = h.client.new_session();
    h.run_until(1, |m| Flooded::answered(m, &second));
    let quiet = new_session_id(&h.msgs, &second);

    // The other session's terminal query is still answered: its input waits behind the flood once, and the query
    // it prints once more.
    h.client.input(&quiet, b"query");
    h.run_until(2, |_| replies.lock().unwrap().starts_with(b"\x1b[?"));

    // And the flood keeps going, a chunk per chunk the runtime has processed and acknowledged.
    for _ in 0..3 * Flooded::BOUND {
        h.step();
    }
    let flooded = h.pushed.output.load(Ordering::SeqCst);
    assert!(
        flooded > 2 * OUTPUT_CREDIT,
        "the flood stalled after {flooded} bytes"
    );
}
