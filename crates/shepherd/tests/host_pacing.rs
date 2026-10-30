//! Host runtime scenarios about load (decision 0013 and the loop's deadlines), on all-fake ports.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use shepherd::host::{CATCH_UP_TICK, RuntimeEvent, SHUTDOWN_GRACE};
use shepherd_core::acpp::Envelope;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LogId, MAX_QUEUED, PtyError, PtyEvent};
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
