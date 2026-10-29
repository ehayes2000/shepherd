//! The host session registry, through the public `shepherd_core::host` API.

use std::collections::{BTreeMap, VecDeque};

use proptest::prelude::*;
use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Envelope, Frame, FrameDraft, Source, SourceKind, Timestamp};
use shepherd_core::host::{Host, HostCommand, HostEvent, HostSession, NewSessionSpec, ReplyTo};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{HostResult, SessionNewOk, WireError};
use shepherd_testkit::builders::{HostDriver, shell_spawn};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// The draft the host appends for a `_shepherd/*` frame in session `id`.
fn draft(id: &str, method: &str, params: Value) -> FrameDraft {
    let frame = Frame::from_value(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
        .unwrap_or_else(|e| panic!("bad frame: {e}"));
    FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        Some(sid(id)),
    )
}

fn append(id: &str, method: &str, params: Value) -> HostCommand {
    HostCommand::Append(Box::new(draft(id, method, params)))
}

fn appends(commands: &[HostCommand]) -> usize {
    commands
        .iter()
        .filter(|c| matches!(c, HostCommand::Append(_)))
        .count()
}

fn names(host: &Host) -> Vec<String> {
    host.sessions().into_iter().map(|s| s.name).collect()
}

fn replay(driver: &HostDriver) -> Host {
    let mut host = Host::new();
    for env in driver.log() {
        host.apply(env);
    }
    host
}

// ---------- new session ----------

#[test]
fn new_session_appends_created_spawns_the_shell_and_replies_with_the_id() {
    let mut host = HostDriver::new();
    let reply = ReplyTo::new(7);
    let cmds = host.handle(HostEvent::NewSession {
        reply,
        id: sid("s1"),
        at: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
        spec: NewSessionSpec {
            name: None,
            spawn: shell_spawn(),
        },
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/session/created",
                json!({ "name": "shell-1" })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: shell_spawn(),
            },
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::SessionNew(SessionNewOk { session: sid("s1") })),
            },
        ]
    );
}

#[test]
fn new_session_uses_the_name_from_the_spec() {
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::NewSession {
        reply,
        id: sid("s1"),
        at: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
        spec: NewSessionSpec {
            name: Some("api".to_owned()),
            spawn: shell_spawn(),
        },
    });
    assert_eq!(
        cmds[0],
        append("s1", "_shepherd/session/created", json!({ "name": "api" }))
    );
    assert_eq!(names(host.host()), ["api"]);
}

#[test]
fn placeholder_names_count_every_session_ever_created() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.new_session("s2");
    host.close("s1");
    host.new_session("s3");
    assert_eq!(names(host.host()), ["shell-3", "shell-2"]);
}

#[test]
fn new_session_with_an_id_already_used_is_an_error_and_appends_nothing() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.close("s1");
    let cmds = host.new_session("s1");
    assert_eq!(appends(&cmds), 0);
    assert!(
        matches!(
            cmds.as_slice(),
            [HostCommand::Reply {
                result: Err(WireError::Internal(_)),
                ..
            }]
        ),
        "{cmds:?}"
    );
}

#[test]
fn handle_never_changes_state_before_apply() {
    let host = Host::new();
    let cmds = host.handle(HostEvent::NewSession {
        reply: ReplyTo::new(1),
        id: sid("s1"),
        at: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
        spec: NewSessionSpec {
            name: None,
            spawn: shell_spawn(),
        },
    });
    assert_eq!(appends(&cmds), 1);
    assert_eq!(host, Host::new());
    assert!(host.sessions().is_empty());
}

// ---------- rename ----------

#[test]
fn rename_appends_renamed_and_acks() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Rename {
        reply,
        session: sid("s1"),
        name: "api".to_owned(),
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/session/renamed",
                json!({ "name": "api", "auto": false })
            ),
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::Ack),
            },
        ]
    );
    assert_eq!(names(host.host()), ["api"]);
}

#[test]
fn renaming_an_unknown_session_replies_unknown_session_and_appends_nothing() {
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Rename {
        reply,
        session: sid("nope"),
        name: "api".to_owned(),
    });
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Err(WireError::UnknownSession {
                session: sid("nope")
            }),
        }]
    );
    assert!(host.log().is_empty());
}

#[test]
fn renaming_a_closed_session_replies_unknown_session() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.close("s1");
    let cmds = host.rename("s1", "api");
    assert_eq!(appends(&cmds), 0);
    assert!(
        matches!(
            cmds.as_slice(),
            [HostCommand::Reply {
                result: Err(WireError::UnknownSession { .. }),
                ..
            }]
        ),
        "{cmds:?}"
    );
}

// ---------- close ----------

#[test]
fn close_kills_the_pty_appends_closed_and_acks() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Close {
        reply,
        session: sid("s1"),
    });
    assert_eq!(
        cmds,
        vec![
            HostCommand::KillPty { session: sid("s1") },
            append(
                "s1",
                "_shepherd/session/closed",
                json!({ "worktree_removed": false })
            ),
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::Ack),
            },
        ]
    );
}

#[test]
fn closing_an_exited_session_does_not_kill_its_pty() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.pty_exited("s1", Some(0));
    let cmds = host.close("s1");
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, HostCommand::KillPty { .. })),
        "{cmds:?}"
    );
    assert_eq!(appends(&cmds), 1);
    assert!(host.host().sessions().is_empty());
}

#[test]
fn closing_an_unknown_session_replies_unknown_session() {
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Close {
        reply,
        session: sid("nope"),
    });
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Err(WireError::UnknownSession {
                session: sid("nope")
            }),
        }]
    );
}

#[test]
fn closing_twice_replies_unknown_session_the_second_time() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.close("s1");
    let cmds = host.close("s1");
    assert_eq!(appends(&cmds), 0);
    assert!(
        matches!(
            cmds.as_slice(),
            [HostCommand::Reply {
                result: Err(WireError::UnknownSession { .. }),
                ..
            }]
        ),
        "{cmds:?}"
    );
}

// ---------- pty exit ----------

#[test]
fn pty_exit_appends_agent_exited() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let cmds = host.pty_exited("s1", Some(3));
    assert_eq!(
        cmds,
        vec![append("s1", "_shepherd/agent/exited", json!({ "code": 3 }))]
    );
    assert_eq!(
        host.host().sessions(),
        [HostSession {
            id: sid("s1"),
            name: "shell-1".to_owned(),
            created: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
            exited: true,
        }]
    );
}

#[test]
fn pty_exit_by_signal_appends_agent_exited_without_a_code() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let cmds = host.pty_exited("s1", None);
    assert_eq!(
        cmds,
        vec![append("s1", "_shepherd/agent/exited", json!({}))]
    );
}

#[test]
fn pty_exit_for_an_unknown_closed_or_exited_session_is_ignored() {
    let mut host = HostDriver::new();
    assert!(host.pty_exited("nope", Some(0)).is_empty());

    host.new_session("s1");
    host.close("s1");
    assert!(host.pty_exited("s1", None).is_empty());

    host.new_session("s2");
    host.pty_exited("s2", Some(0));
    assert!(host.pty_exited("s2", Some(0)).is_empty());
}

#[test]
fn an_agent_exiting_inside_the_shell_does_not_mark_the_pty_exited() {
    // A hook-sourced `agent/exited` is about an agent the shell ran, not about the PTY process.
    let mut host = HostDriver::new();
    host.new_session("s1");
    let mut replayed = replay(&host);
    let hook = shepherd_testkit::builders::EnvelopeBuilder::new()
        .session(sid("s1"))
        .internal(json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/exited", "params": { "code": 0 } }));
    replayed.apply(&hook);
    assert!(!replayed.sessions()[0].exited);
    assert!(
        replayed
            .handle(HostEvent::Close {
                reply: ReplyTo::new(1),
                session: sid("s1"),
            })
            .contains(&HostCommand::KillPty { session: sid("s1") })
    );
}

// ---------- input and resize ----------

#[test]
fn input_and_resize_go_to_a_live_sessions_pty() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let size = PtySize {
        cols: 120,
        rows: 40,
    };
    assert_eq!(
        host.handle(HostEvent::Input {
            session: sid("s1"),
            bytes: b"ls\r".to_vec(),
        }),
        vec![HostCommand::WritePty {
            session: sid("s1"),
            bytes: b"ls\r".to_vec(),
        }]
    );
    assert_eq!(
        host.handle(HostEvent::Resize {
            session: sid("s1"),
            size,
        }),
        vec![HostCommand::ResizePty {
            session: sid("s1"),
            size,
        }]
    );
}

#[test]
fn input_and_resize_for_unknown_closed_or_exited_sessions_are_dropped() {
    let mut host = HostDriver::new();
    host.new_session("closed");
    host.close("closed");
    host.new_session("exited");
    host.pty_exited("exited", Some(0));
    for id in ["nope", "closed", "exited"] {
        assert!(
            host.handle(HostEvent::Input {
                session: sid(id),
                bytes: b"x".to_vec(),
            })
            .is_empty(),
            "{id}"
        );
        assert!(
            host.handle(HostEvent::Resize {
                session: sid(id),
                size: PtySize { cols: 1, rows: 1 },
            })
            .is_empty(),
            "{id}"
        );
    }
}

// ---------- listing ----------

#[test]
fn sessions_are_listed_newest_first_without_closed_ones() {
    let mut host = HostDriver::new();
    host.new_session("a");
    host.new_session("b");
    host.new_session("c");
    host.rename("a", "first");
    host.close("b");
    let ids: Vec<_> = host.host().sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [sid("c"), sid("a")]);
    assert_eq!(names(host.host()), ["shell-3", "first"]);
}

#[test]
fn entries_the_registry_does_not_read_change_nothing() {
    let mut host = Host::new();
    let mut log = shepherd_testkit::builders::EnvelopeBuilder::new().session(sid("s1"));
    host.apply(&log.to_agent(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
        "params": { "sessionId": "a1", "prompt": [] } }),
    ));
    host.apply(&log.internal(
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/renamed", "params": { "name": "x" } }),
    ));
    host.apply(&log.internal(json!({ "jsonrpc": "2.0", "method": "_vendor/thing", "params": {} })));
    assert_eq!(host, Host::new());
}

// ---------- replay ----------

#[test]
fn replaying_the_appended_frames_rebuilds_the_same_state() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.rename("s1", "api");
    host.new_session("s2");
    host.pty_exited("s2", Some(1));
    host.new_session("s3");
    host.close("s2");
    host.close("s3");
    host.rename("s1", "web");
    assert_eq!(replay(&host), *host.host());
}

#[derive(Debug, Clone)]
enum Op {
    New(u8),
    Rename(u8, u8),
    Close(u8),
    Exit(u8, Option<i32>),
    Input(u8),
    Resize(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let id = 0u8..5;
    prop_oneof![
        id.clone().prop_map(Op::New),
        (id.clone(), 0u8..3).prop_map(|(i, n)| Op::Rename(i, n)),
        id.clone().prop_map(Op::Close),
        (id.clone(), proptest::option::of(-1i32..3)).prop_map(|(i, c)| Op::Exit(i, c)),
        id.clone().prop_map(Op::Input),
        id.prop_map(Op::Resize),
    ]
}

fn run(ops: Vec<Op>) -> HostDriver {
    let mut host = HostDriver::new();
    let id = |i: u8| format!("s{i}");
    for op in ops {
        match op {
            Op::New(i) => {
                host.new_session(&id(i));
            }
            Op::Rename(i, n) => {
                host.rename(&id(i), &format!("name-{n}"));
            }
            Op::Close(i) => {
                host.close(&id(i));
            }
            Op::Exit(i, code) => {
                host.pty_exited(&id(i), code);
            }
            Op::Input(i) => {
                host.handle(HostEvent::Input {
                    session: sid(&id(i)),
                    bytes: vec![b'x'],
                });
            }
            Op::Resize(i) => {
                host.handle(HostEvent::Resize {
                    session: sid(&id(i)),
                    size: PtySize { cols: 10, rows: 5 },
                });
            }
        }
    }
    host
}

/// The appended envelopes split into their logs (the host log and one per session), each in seq order.
fn logs(driver: &HostDriver) -> Vec<VecDeque<Envelope>> {
    let mut logs: BTreeMap<Option<SessionId>, VecDeque<Envelope>> = BTreeMap::new();
    for env in driver.log() {
        logs.entry(env.session().cloned())
            .or_default()
            .push_back(env.clone());
    }
    logs.into_values().collect()
}

/// Folds `logs` into a fresh host, taking each next entry from the log `picks` chooses, so every log keeps its own
/// order while the interleaving between logs follows `picks`.
fn replay_interleaved(mut logs: Vec<VecDeque<Envelope>>, picks: &[u16]) -> Host {
    let mut host = Host::new();
    let mut picks = picks.iter().cycle();
    loop {
        logs.retain(|log| !log.is_empty());
        if logs.is_empty() {
            return host;
        }
        let pick = usize::from(picks.next().copied().unwrap_or(0)) % logs.len();
        if let Some(env) = logs[pick].pop_front() {
            host.apply(&env);
        }
    }
}

#[test]
fn sessions_are_ordered_by_created_ts_whatever_order_their_logs_are_replayed_in() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    driver.new_session("s2");
    let mut logs = logs(&driver);
    logs.reverse();
    let mut host = Host::new();
    for env in logs.into_iter().flatten() {
        host.apply(&env);
    }
    assert_eq!(names(&host), ["shell-2", "shell-1"]);
}

#[test]
fn sessions_created_at_the_same_ts_are_ordered_by_id() {
    let mut host = HostDriver::new();
    host.pin_ts("2026-10-29T12:00:00.000Z");
    host.new_session("b");
    host.new_session("a");
    host.new_session("c");
    let ids: Vec<_> = host.host().sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [sid("c"), sid("b"), sid("a")]);
}

proptest! {
    #[test]
    fn replay_in_any_interleaving_of_the_logs_equals_live_state(
        ops in proptest::collection::vec(op(), 0..60),
        interleavings in proptest::collection::vec(proptest::collection::vec(any::<u16>(), 1..40), 1..6),
    ) {
        let host = run(ops);
        for picks in &interleavings {
            let replayed = replay_interleaved(logs(&host), picks);
            prop_assert_eq!(replayed.sessions(), host.host().sessions());
            prop_assert_eq!(&replayed, host.host());
        }
    }

    #[test]
    fn replay_equals_live_state_for_any_sequence(ops in proptest::collection::vec(op(), 0..60)) {
        let host = run(ops);
        prop_assert_eq!(replay(&host), host.host().clone());
        let closed = host.log().iter().filter(|e| e.frame().method() == Some("_shepherd/session/closed"));
        for env in closed {
            let id = env.session().cloned();
            prop_assert!(host.host().sessions().iter().all(|s| Some(&s.id) != id.as_ref()));
        }
    }
}
