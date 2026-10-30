//! Host runtime scenarios (T11), on all-fake ports. See `support` for the harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use shepherd::host::RuntimeEvent;
use shepherd_core::acpp::{Frame, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{EventLog, LogError, LogId, PtyError, PtySize};
use shepherd_core::wire::{
    ClientRequest, Hello, HostMessage, PROTOCOL_VERSION, SessionClose, SessionRename,
    SubscribeFrom, WireError,
};
use shepherd_term::Screen;
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::pty::ProgramInput;
use support::*;

fn method(env: &shepherd_core::acpp::Envelope) -> String {
    match env.frame() {
        Frame::Shepherd(m) => m.body().method().to_owned(),
        other => format!("{other:?}"),
    }
}

fn text(screen: &Screen, row: u16) -> String {
    screen
        .row(row)
        .map(|c| c.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A client that has created a session and subscribed, with its inbox drained.
fn with_session(h: &mut HostHarness) -> (FakeClient, SessionId) {
    let mut client = h.connect();
    client.subscribe_all();
    let req = client.new_session();
    h.step();
    let id = new_session_id(&client.take(), &req);
    (client, id)
}

// ---------- the main flow ----------

#[test]
fn client_creates_a_session_types_into_it_and_sees_output_and_log_entries() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.subscribe_all();
    let req = client.new_session();
    h.step();
    let msgs = client.take();
    let id = new_session_id(&msgs, &req);
    let created = entries(&msgs);
    assert_eq!(created.len(), 1);
    assert_eq!(method(&created[0]), "_shepherd/session/created");
    assert_eq!(created[0].session(), Some(&id));

    client.input(&id, b"hello");
    h.step();
    assert_eq!(output(&client.take(), &id), b"hello");
}

#[test]
fn after_a_restart_the_client_sees_the_same_session_list() {
    let mut h = HostHarness::new();
    let (mut client, first) = with_session(&mut h);
    let req = client.request(ClientRequest::SessionRename(SessionRename {
        session: first.clone(),
        name: "api".into(),
    }));
    client.new_session();
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    let before: Vec<_> = h
        .host()
        .sessions()
        .iter()
        .map(|s| (s.id.clone(), s.name.clone()))
        .collect();
    assert_eq!(before.len(), 2);

    h.restart();
    assert!(client.is_disconnected());
    let mut client = h.connect();
    client.subscribe_all();
    h.step();
    let seen = fold(&entries(&client.take()));
    let after: Vec<_> = seen
        .sessions()
        .iter()
        .map(|s| (s.id.clone(), s.name.clone()))
        .collect();
    assert_eq!(after, before);
}

#[test]
fn a_fresh_host_replayed_from_the_log_equals_the_live_one() {
    let mut h = HostHarness::new();
    let (mut client, first) = with_session(&mut h);
    client.new_session();
    client.input(&first, b"exit\r");
    let second = {
        h.step();
        let msgs = client.take();
        entries(&msgs)
            .iter()
            .filter_map(|e| e.session().cloned())
            .find(|s| *s != first)
            .expect("second session")
    };
    client.request(ClientRequest::SessionRename(SessionRename {
        session: second.clone(),
        name: "docs".into(),
    }));
    client.request(ClientRequest::SessionClose(SessionClose {
        session: second,
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
    }));
    h.step();

    assert_eq!(&h.replayed(), h.host());
    assert_eq!(h.host().sessions().len(), 1);
    assert!(h.host().sessions()[0].exited);
}

// ---------- startup ----------

#[test]
fn on_start_sessions_whose_pty_is_gone_become_exited() {
    // A previous host created two sessions; one exited, the other was still running when the host died.
    let mut prev = HostDriver::new();
    prev.new_session("s_a");
    prev.new_session("s_b");
    prev.pty_exited("s_a", Some(0));
    let mut log = FakeEventLog::new();
    for id in ["s_a", "s_b"] {
        let entries: Vec<_> = prev
            .log()
            .iter()
            .filter(|e| e.session().map(SessionId::as_str) == Some(id))
            .cloned()
            .collect();
        log = log.with_envelopes(LogId::Session(SessionId::new(id)), entries);
    }

    let h = HostHarness::builder().log(log).start();
    assert!(h.host().sessions().iter().all(|s| s.exited));
    let b = h
        .log()
        .read(LogId::Session(SessionId::new("s_b")), Seq::ZERO)
        .unwrap();
    assert_eq!(method(b.last().unwrap()), "_shepherd/agent/exited");
    // s_a already had its exit: no second one.
    let a = h
        .log()
        .read(LogId::Session(SessionId::new("s_a")), Seq::ZERO)
        .unwrap();
    assert_eq!(
        a.iter()
            .filter(|e| method(e) == "_shepherd/agent/exited")
            .count(),
        1
    );
}

// ---------- executing commands ----------

#[test]
fn appended_entries_are_stamped_with_id_ts_and_the_local_machine() {
    let mut h = HostHarness::new();
    let (_client, id) = with_session(&mut h);
    let env = &h.log().read(LogId::Session(id), Seq::ZERO).unwrap()[0];
    assert_eq!(env.id().as_str(), "e1");
    assert_eq!(env.ts().as_str(), "2026-10-29T12:00:00.002Z");
    assert_eq!(env.machine(), &MachineId::new("local"));
    assert_eq!(env.seq(), Seq::new(1));
}

#[test]
fn new_sessions_run_the_shell_profile_in_home_at_80x24() {
    let seen = Arc::new(Mutex::new(None));
    let record = seen.clone();
    let mut h = HostHarness::builder()
        .program(
            SHELL,
            Arc::new(move |input, p| {
                if input == ProgramInput::Start {
                    *record.lock().unwrap() = Some(p.spec().clone());
                }
            }),
        )
        .start();
    with_session(&mut h);
    let spec = seen.lock().unwrap().clone().expect("spawned");
    assert_eq!(spec.command, SHELL);
    assert_eq!(spec.cwd, std::path::PathBuf::from(HOME));
    assert_eq!(spec.size, PtySize { cols: 80, rows: 24 });
}

#[test]
fn session_ids_come_from_the_env() {
    let mut h = HostHarness::new();
    let (_client, id) = with_session(&mut h);
    assert_eq!(id, SessionId::new("s_t1"));
}

#[test]
fn a_session_whose_pty_fails_to_spawn_shows_as_exited() {
    let mut h = HostHarness::new();
    h.runtime_mut()
        .pty_mut()
        .fail_next(PtyError::SpawnFailed("no such file".into()));
    let (_client, id) = with_session(&mut h);
    assert!(h.host().sessions()[0].exited);
    let log = h.log().read(LogId::Session(id), Seq::ZERO).unwrap();
    assert_eq!(method(log.last().unwrap()), "_shepherd/agent/exited");
}

#[test]
fn a_failed_append_skips_the_rest_of_the_batch_and_replies_internal() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    h.log().fail_next(LogError::Io("disk full".into()));
    let req = client.new_session();
    h.step();
    let msgs = client.take();
    assert!(matches!(response(&msgs, &req), Err(WireError::Internal(_))));
    assert!(h.host().sessions().is_empty());
    assert!(!h.runtime().pty().is_running(&SessionId::new("s_t1")));
}

#[test]
fn resize_resizes_the_pty() {
    let mut h = HostHarness::new();
    let (mut client, id) = with_session(&mut h);
    client.resize(&id, 120, 40);
    h.step();
    assert_eq!(
        h.runtime().pty().size(&id),
        Some(PtySize {
            cols: 120,
            rows: 40
        })
    );
}

#[test]
fn resize_resizes_the_host_screen_too() {
    let stdin = Arc::new(Mutex::new(Vec::new()));
    let record = stdin.clone();
    // Prints 100 columns, then asks where the cursor is. The host's screen answers.
    let mut h = HostHarness::builder()
        .program(
            SHELL,
            Arc::new(move |input, p| match input {
                ProgramInput::Resized => p.print(format!("{}\x1b[6n", "x".repeat(100))),
                ProgramInput::Stdin(bytes) => record.lock().unwrap().extend_from_slice(bytes),
                ProgramInput::Start => {}
            }),
        )
        .start();
    let (mut client, id) = with_session(&mut h);
    client.resize(&id, 120, 40);
    h.step();
    // At 80 columns the line would have wrapped: row 2, column 21.
    assert_eq!(stdin.lock().unwrap().as_slice(), b"\x1b[1;101R");
}

#[test]
fn closing_a_session_kills_its_pty() {
    let mut h = HostHarness::new();
    let (mut client, id) = with_session(&mut h);
    let req = client.request(ClientRequest::SessionClose(SessionClose {
        session: id.clone(),
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
    }));
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert!(!h.runtime().pty().is_running(&id));
    assert!(h.host().sessions().is_empty());
}

#[test]
fn errors_from_core_are_replied_to_the_requesting_client() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let req = client.request(ClientRequest::SessionRename(SessionRename {
        session: SessionId::new("s_nope"),
        name: "x".into(),
    }));
    h.step();
    assert_eq!(
        response(&client.take(), &req),
        Err(WireError::UnknownSession {
            session: SessionId::new("s_nope")
        })
    );
}

// ---------- wire ----------

#[test]
fn hello_is_answered_with_the_host_version_machine_and_profiles() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let req = client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: None,
    }));
    h.step();
    assert_eq!(
        response(&client.take(), &req),
        Ok(json!({
            "version": PROTOCOL_VERSION,
            "machine": "local",
            "profiles": [
                { "name": "claude", "agent": "claude-code" },
                { "name": "codex", "agent": "codex" },
                { "name": "shell", "agent": "shell" },
            ],
            "default_profile": "shell",
        }))
    );
}

#[test]
fn hello_tells_the_client_to_ring_the_bell_when_the_config_says_so() {
    let config = shepherd_core::config::RawConfig {
        notify: Some(shepherd_core::config::RawNotify { bell: Some(true) }),
        ..shell_default_config()
    };
    let mut h = HostHarness::builder().config(config).start();
    let mut client = h.connect();
    let req = client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: None,
    }));
    h.step();
    let result = response(&client.take(), &req).expect("hello ok");
    assert_eq!(result["notify"], json!({ "bell": true }));
}

#[test]
fn hello_with_another_version_is_a_version_mismatch() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let req = client.request(ClientRequest::Hello(Hello {
        version: 99,
        machine: MachineId::new("laptop"),
        cwd: None,
    }));
    h.step();
    assert_eq!(
        response(&client.take(), &req),
        Err(WireError::VersionMismatch {
            host: PROTOCOL_VERSION,
            client: 99
        })
    );
}

#[test]
fn an_unreadable_line_is_answered_with_a_null_id() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.send_raw(b"{not json");
    h.step();
    let msgs = client.take();
    assert!(matches!(
        response(&msgs, &shepherd_core::acpp::RequestId::Null),
        Err(WireError::Parse(_))
    ));
}

#[test]
fn a_bad_notification_is_never_answered() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.send_raw(br#"{"jsonrpc":"2.0","method":"_shepherd/session/input","params":{}}"#);
    h.step();
    assert!(client.take().is_empty());
}

// ---------- subscriptions and fan-out ----------

#[test]
fn unsubscribed_clients_get_no_entries_or_output() {
    let mut h = HostHarness::new();
    let (mut a, id) = with_session(&mut h);
    let mut b = h.connect();
    a.input(&id, b"hi");
    h.step();
    assert!(b.take().is_empty());
}

#[test]
fn subscribe_from_a_seq_replays_the_rest_then_goes_live_with_no_gap_or_duplicate() {
    let mut h = HostHarness::new();
    let (mut a, id) = with_session(&mut h);
    for name in ["one", "two", "three"] {
        a.request(ClientRequest::SessionRename(SessionRename {
            session: id.clone(),
            name: name.into(),
        }));
    }
    h.step();
    // Seqs 1 (created) .. 4 (renamed three).

    let mut b = h.connect();
    let mut from = SubscribeFrom {
        host: None,
        ..Default::default()
    };
    from.sessions.insert(id.clone(), Seq::new(2));
    let req = b.subscribe(from);
    // Queued behind the subscribe in the same channel: a live entry right after the catch-up.
    a.request(ClientRequest::SessionRename(SessionRename {
        session: id.clone(),
        name: "four".into(),
    }));
    h.step();

    let msgs = b.take();
    let seqs: Vec<u64> = entries(&msgs).iter().map(|e| e.seq().get()).collect();
    assert_eq!(seqs, [3, 4, 5]);
    // The ack follows the catch-up.
    let ack = msgs
        .iter()
        .position(|m| matches!(m, HostMessage::Response(r) if r.id == req))
        .unwrap();
    let last_catch_up = msgs
        .iter()
        .rposition(|m| {
            entries(std::slice::from_ref(m))
                .first()
                .is_some_and(|e| e.seq().get() == 4)
        })
        .unwrap();
    assert!(ack > last_catch_up);
}

#[test]
fn a_subscribe_whose_catch_up_fails_sends_nothing_and_replies_internal() {
    let mut h = HostHarness::new();
    let (_a, id) = with_session(&mut h);
    let mut b = h.connect();
    h.log().fail_next(LogError::Io("disk gone".into()));
    let req = b.subscribe_all();
    h.step();
    let msgs = b.take();
    assert!(matches!(response(&msgs, &req), Err(WireError::Internal(_))));
    assert!(entries(&msgs).is_empty());
    assert!(output(&msgs, &id).is_empty());
}

#[test]
fn every_subscriber_gets_new_entries_and_output() {
    let mut h = HostHarness::new();
    let (mut a, id) = with_session(&mut h);
    let mut b = h.connect();
    b.subscribe_all();
    h.step();
    b.take();
    a.input(&id, b"x");
    a.request(ClientRequest::SessionRename(SessionRename {
        session: id.clone(),
        name: "n".into(),
    }));
    h.step();
    for client in [&mut a, &mut b] {
        let msgs = client.take();
        assert_eq!(output(&msgs, &id), b"x");
        assert_eq!(entries(&msgs).len(), 1);
    }
}

#[test]
fn a_client_attaching_mid_session_sees_the_current_screen_then_live_output() {
    let mut h = HostHarness::new();
    let (mut a, id) = with_session(&mut h);
    a.input(&id, b"hello\r\nworld");
    h.step();

    let mut b = h.connect();
    b.subscribe_all();
    h.step();
    let msgs = b.take();
    let first_output = msgs
        .iter()
        .position(|m| !output(std::slice::from_ref(m), &id).is_empty())
        .expect("snapshot");
    let mut screen = Screen::new(80, 24);
    screen.feed(&output(&msgs[first_output..=first_output], &id));
    assert_eq!(text(&screen, 0), "hello");
    assert_eq!(text(&screen, 1), "world");

    a.input(&id, b"!");
    h.step();
    screen.feed(&output(&b.take(), &id));
    assert_eq!(text(&screen, 1), "world!");
}

#[test]
fn a_disconnected_client_is_forgotten() {
    let mut h = HostHarness::new();
    let (mut a, id) = with_session(&mut h);
    let b = h.connect();
    drop(b);
    a.input(&id, b"still works");
    h.step();
    assert_eq!(output(&a.take(), &id), b"still works");
}

// ---------- screens ----------

/// A program that asks for the primary device attributes on start and records what it reads back.
fn querying_program(query: &'static [u8], stdin: Arc<Mutex<Vec<u8>>>) -> Program {
    Arc::new(move |input, p| match input {
        ProgramInput::Start => p.print(query),
        ProgramInput::Stdin(bytes) => stdin.lock().unwrap().extend_from_slice(bytes),
        ProgramInput::Resized => {}
    })
}

#[test]
fn terminal_queries_are_answered_with_no_client_attached() {
    let stdin = Arc::new(Mutex::new(Vec::new()));
    let mut h = HostHarness::builder()
        .program(SHELL, querying_program(b"\x1b[c", stdin.clone()))
        .start();
    let mut client = h.connect();
    client.new_session();
    h.step();
    let answer = stdin.lock().unwrap().clone();
    assert!(answer.starts_with(b"\x1b[?"), "{answer:?}");
}

#[test]
fn output_held_by_a_synchronized_update_is_released_when_its_deadline_passes() {
    let stdin = Arc::new(Mutex::new(Vec::new()));
    // The DA1 query sits inside an update that never ends, so it is only answered once the hold times out.
    let mut h = HostHarness::builder()
        .program(SHELL, querying_program(b"\x1b[?2026h\x1b[c", stdin.clone()))
        .start();
    let mut client = h.connect();
    client.new_session();
    h.step();
    assert!(stdin.lock().unwrap().is_empty());
    assert!(h.runtime().next_deadline().is_some());

    h.clock().advance(Duration::from_secs(5));
    h.step();
    assert!(stdin.lock().unwrap().starts_with(b"\x1b[?"));
    assert_eq!(h.runtime().next_deadline(), None);
}

// ---------- shutdown ----------

#[test]
fn shutdown_kills_ptys_appends_their_exits_and_disconnects_clients() {
    let mut h = HostHarness::new();
    let (mut client, id) = with_session(&mut h);
    h.send(RuntimeEvent::Shutdown);
    assert!(h.runtime().is_finished());
    assert!(!h.runtime().pty().is_running(&id));
    let log = h.log().read(LogId::Session(id), Seq::ZERO).unwrap();
    assert_eq!(method(log.last().unwrap()), "_shepherd/agent/exited");
    assert!(
        matches!(log.last().unwrap().frame(), Frame::Shepherd(m) if matches!(m.body(), ShepherdFrame::AgentExited(e) if e.code.is_none()))
    );
    assert!(client.is_disconnected());
}

#[test]
fn requests_while_shutdown_waits_for_exits_are_not_acted_on() {
    let mut h = HostHarness::new();
    let (mut client, _id) = with_session(&mut h);
    // The PTY's exit is queued behind this request, so shutdown is still waiting for it when the request arrives.
    h.runtime().sender().send(RuntimeEvent::Shutdown);
    client.new_session();
    h.step();
    assert_eq!(h.host().sessions().len(), 1);
}

#[test]
fn port_sinks_never_block_even_when_the_loop_is_not_running() {
    let h = HostHarness::new();
    let tx = h.runtime().sender();
    let sink = tx.pty_sink();
    for _ in 0..100_000 {
        sink.send(shepherd_core::ports::PtyEvent::Output {
            session: SessionId::new("s_x"),
            bytes: vec![b'x'],
        });
    }
}
