//! The host session registry, through the public `shepherd_core::host` API.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use proptest::prelude::*;
use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Envelope, Frame, FrameDraft, Source, SourceKind, Timestamp};
use shepherd_core::config::AgentKind;
use shepherd_core::host::{
    AgentTranscript, Host, HostCommand, HostEvent, HostSession, NewSessionSpec, ReplyTo,
    ResumeSpec, ResumeTarget, ResumeUnavailable, WorkspaceAsk, expand_home,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{HostResult, InputKind, SessionNewOk, WireError};
use shepherd_testkit::builders::{
    HostDriver, claude_resume_spec, claude_session_spec, shell_session_spec, shell_spawn,
};

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
        spec: shell_session_spec(),
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/session/created",
                json!({ "name": "shell-1", "agent": "shell", "profile": "shell", "cwd": "/home/test" })
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
            ..shell_session_spec()
        },
    });
    assert_eq!(
        cmds[0],
        append(
            "s1",
            "_shepherd/session/created",
            json!({ "name": "api", "agent": "shell", "profile": "shell", "cwd": "/home/test" })
        )
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
fn placeholder_names_follow_the_profile_and_created_records_the_agent_kind() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let reply = host.reply_to();
    let at = host.now();
    let cmds = host.handle(HostEvent::NewSession {
        reply,
        id: sid("s2"),
        at,
        spec: NewSessionSpec {
            profile: "claude".to_owned(),
            agent: AgentKind::ClaudeCode,
            ..shell_session_spec()
        },
    });
    assert_eq!(
        cmds[0],
        append(
            "s2",
            "_shepherd/session/created",
            json!({ "name": "claude-2", "agent": "claude-code", "profile": "claude", "cwd": "/home/test" })
        )
    );
    assert_eq!(names(host.host()), ["claude-2", "shell-1"]);
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
        spec: shell_session_spec(),
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

// ---------- hooks ----------

fn hook(host: &mut HostDriver, id: &str, agent: AgentKind) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Hook {
        reply,
        session: sid(id),
        agent,
        payload: json!({ "hook_event_name": "Stop" }),
    });
    (reply, cmds)
}

#[test]
fn a_hook_for_an_open_session_is_translated_then_acked() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let (reply, cmds) = hook(&mut host, "s1", AgentKind::ClaudeCode);
    assert_eq!(
        cmds,
        vec![
            HostCommand::TranslateHook {
                session: sid("s1"),
                agent: AgentKind::ClaudeCode,
                payload: json!({ "hook_event_name": "Stop" }),
            },
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::Ack),
            },
        ]
    );
}

#[test]
fn a_hook_for_an_exited_session_is_still_translated() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.pty_exited("s1", Some(0));
    let (_, cmds) = hook(&mut host, "s1", AgentKind::ClaudeCode);
    assert!(
        matches!(cmds[0], HostCommand::TranslateHook { .. }),
        "{cmds:?}"
    );
}

#[test]
fn a_hook_for_an_unknown_or_closed_session_is_refused() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.close("s1");
    for id in ["s1", "nope"] {
        let (reply, cmds) = hook(&mut host, id, AgentKind::ClaudeCode);
        assert_eq!(
            cmds,
            vec![HostCommand::Reply {
                to: reply,
                result: Err(WireError::UnknownSession { session: sid(id) }),
            }]
        );
    }
}

#[test]
fn a_hook_from_an_agent_kind_without_a_translator_is_invalid_params() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let (reply, cmds) = hook(&mut host, "s1", AgentKind::Shell);
    assert!(
        matches!(
            &cmds[..],
            [HostCommand::Reply { to, result: Err(WireError::InvalidParams(_)) }] if *to == reply
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
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        hooks: shepherd_core::config::WorkspaceHooks::default(),
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
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        hooks: shepherd_core::config::WorkspaceHooks::default(),
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
            machine: shepherd_core::ids::MachineId::new("m_test"),
            created: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
            exited: true,
            resume: Err(ResumeUnavailable::NotAnAgent),
            workspace: None,
            failed: None,
            creating_worktree: false,
            warning: None,
            worktree: None,
            branch: None,
            removing_worktree: false,
            pr: None,
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
                worktree: shepherd_core::wire::CloseWorktree::IfSafe,
                hooks: shepherd_core::config::WorkspaceHooks::default(),
            })
            .contains(&HostCommand::KillPty { session: sid("s1") })
    );
}

// ---------- resume ----------

fn resume_target(host: &HostDriver, id: &str) -> Option<Result<ResumeTarget, ResumeUnavailable>> {
    host.host().resume_target(&sid(id))
}

/// A Claude Code session `id` whose agent reported `agent_session` and then exited.
fn exited_claude(host: &mut HostDriver, id: &str, agent_session: Option<&str>) {
    host.new_session_with(id, claude_session_spec());
    host.agent_started(id, "claude-code", agent_session);
    host.pty_exited(id, Some(0));
}

#[test]
fn the_agent_session_id_is_captured_from_agent_started() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    assert_eq!(
        resume_target(&host, "s1"),
        Some(Ok(ResumeTarget {
            profile: "claude".to_owned(),
            agent: AgentKind::ClaudeCode,
            cwd: "/home/test".into(),
            agent_session_id: "abc123".to_owned(),
        }))
    );
    assert_eq!(host.host().sessions()[0].resume, Ok(()));
}

// ---------- agent transcript ----------

fn transcript(host: &HostDriver, id: &str) -> Option<AgentTranscript> {
    host.host().agent_transcript(&sid(id))
}

#[test]
fn the_transcript_path_from_a_hook_reaches_the_host() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript(
        "s1",
        "claude-code",
        "abc123",
        "/h/.claude/projects/-w/abc123.jsonl",
    );
    assert_eq!(
        transcript(&host, "s1"),
        Some(AgentTranscript {
            agent: AgentKind::ClaudeCode,
            agent_session_id: "abc123".to_owned(),
            transcript_path: "/h/.claude/projects/-w/abc123.jsonl".into(),
        })
    );
}

#[test]
fn a_session_whose_agent_reported_no_transcript_path_has_no_transcript() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started("s1", "claude-code", Some("abc123"));
    assert_eq!(transcript(&host, "s1"), None);
}

#[test]
fn a_new_agent_session_without_a_path_forgets_the_old_sessions_path() {
    // `/clear` starts a new agent session: the old transcript is not the new session's.
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript("s1", "claude-code", "first", "/t/first.jsonl");
    host.agent_started("s1", "claude-code", Some("second"));
    assert_eq!(transcript(&host, "s1"), None);
}

#[test]
fn a_resume_of_the_same_agent_session_keeps_its_transcript_path() {
    // The host's own `agent/started` on resume names the agent session but not its transcript.
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript("s1", "claude-code", "abc123", "/t/abc123.jsonl");
    host.pty_exited("s1", Some(0));
    let cmds = host.resume("s1", claude_resume_spec());
    assert!(matches!(cmds.first(), Some(HostCommand::Append(_))));
    assert_eq!(
        transcript(&host, "s1").map(|t| t.transcript_path),
        Some("/t/abc123.jsonl".into())
    );
}

#[test]
fn the_latest_transcript_path_wins() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript("s1", "claude-code", "first", "/t/first.jsonl");
    host.agent_started_with_transcript("s1", "claude-code", "second", "/t/second.jsonl");
    let t = transcript(&host, "s1").expect("a transcript");
    assert_eq!(
        (t.agent_session_id.as_str(), t.transcript_path),
        ("second", "/t/second.jsonl".into())
    );
}

#[test]
fn a_shell_session_has_no_agent_transcript() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.agent_started_with_transcript("s1", "claude-code", "abc123", "/t/abc123.jsonl");
    assert_eq!(transcript(&host, "s1"), None);
}

#[test]
fn a_closed_session_has_no_agent_transcript() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript("s1", "claude-code", "abc123", "/t/abc123.jsonl");
    host.close("s1");
    assert_eq!(transcript(&host, "s1"), None);
}

#[test]
fn the_latest_agent_session_id_is_the_one_resumed() {
    // `/clear` in Claude Code starts a new agent session in the same process.
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started("s1", "claude-code", Some("first"));
    host.agent_started("s1", "claude-code", Some("second"));
    host.agent_started("s1", "claude-code", None);
    host.pty_exited("s1", Some(0));
    let target = resume_target(&host, "s1").and_then(Result::ok);
    assert_eq!(
        target.map(|t| t.agent_session_id),
        Some("second".to_owned())
    );
}

#[test]
fn a_running_session_cannot_be_resumed() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started("s1", "claude-code", Some("abc123"));
    assert_eq!(
        resume_target(&host, "s1"),
        Some(Err(ResumeUnavailable::Running))
    );
    assert_eq!(
        host.host().sessions()[0].resume,
        Err(ResumeUnavailable::Running)
    );
}

#[test]
fn a_session_with_no_captured_agent_session_id_cannot_be_resumed() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", None);
    assert_eq!(
        resume_target(&host, "s1"),
        Some(Err(ResumeUnavailable::NoAgentSession))
    );
}

#[test]
fn a_shell_session_cannot_be_resumed() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.agent_started("s1", "claude-code", Some("abc123"));
    host.pty_exited("s1", Some(0));
    assert_eq!(
        resume_target(&host, "s1"),
        Some(Err(ResumeUnavailable::NotAnAgent))
    );
}

#[test]
fn a_session_created_before_resume_was_recorded_cannot_be_resumed() {
    // Its `session/created` has no profile or cwd.
    let mut host = HostDriver::new();
    let created = json!({ "jsonrpc": "2.0", "method": "_shepherd/session/created",
        "params": { "name": "old", "agent": "claude-code" } });
    host.append(Some("s1"), Source::new(SourceKind::Shepherd, None), created);
    host.agent_started("s1", "claude-code", Some("abc123"));
    host.pty_exited("s1", Some(0));
    assert_eq!(
        resume_target(&host, "s1"),
        Some(Err(ResumeUnavailable::NotRecorded))
    );
}

#[test]
fn a_hook_reporting_an_agent_start_after_the_pty_exited_does_not_mark_it_running() {
    // Only the host's own `agent/started` (a resume) means its PTY runs again.
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    host.agent_started("s1", "claude-code", Some("later"));
    let listed = &host.host().sessions()[0];
    assert!(listed.exited);
    assert_eq!(listed.resume, Ok(()));
    let target = resume_target(&host, "s1").and_then(Result::ok);
    assert_eq!(target.map(|t| t.agent_session_id), Some("later".to_owned()));
}

#[test]
fn resume_target_of_an_unknown_or_closed_session_is_none() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    host.close("s1");
    assert_eq!(resume_target(&host, "s1"), None);
    assert_eq!(resume_target(&host, "nope"), None);
}

#[test]
fn resume_appends_agent_started_spawns_the_agent_and_acks() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::Resume {
        reply,
        session: sid("s1"),
        spec: claude_resume_spec(),
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/agent/started",
                json!({ "agent": "claude-code", "agent_session_id": "abc123" })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: claude_resume_spec().spawn,
            },
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::Ack),
            },
        ]
    );
    let listed = &host.host().sessions()[0];
    assert!(!listed.exited);
    assert_eq!(listed.resume, Err(ResumeUnavailable::Running));
    assert_eq!(replay(&host), *host.host());
}

#[test]
fn a_resumed_session_takes_input_and_records_its_next_exit() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    host.resume("s1", claude_resume_spec());
    let input = host.handle(HostEvent::Input {
        session: sid("s1"),
        bytes: b"x".to_vec(),
        kind: InputKind::Keys,
    });
    assert_eq!(input.len(), 1, "{input:?}");
    assert_eq!(appends(&host.pty_exited("s1", Some(0))), 1);
    assert_eq!(resume_target(&host, "s1").map(|r| r.is_ok()), Some(true));
}

#[test]
fn resuming_a_session_that_cannot_be_resumed_says_why_and_does_nothing() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", None);
    host.new_session_with("s2", claude_session_spec());
    for id in ["s1", "s2"] {
        let cmds = host.resume(id, claude_resume_spec());
        assert!(
            matches!(
                cmds.as_slice(),
                [HostCommand::Reply {
                    result: Err(WireError::InvalidParams(_)),
                    ..
                }]
            ),
            "{id}: {cmds:?}"
        );
    }
}

#[test]
fn resuming_an_unknown_session_replies_unknown_session() {
    let mut host = HostDriver::new();
    let cmds = host.resume("nope", claude_resume_spec());
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

#[test]
fn resuming_with_a_profile_that_is_not_the_sessions_is_refused() {
    let mut host = HostDriver::new();
    exited_claude(&mut host, "s1", Some("abc123"));
    let other_profile = ResumeSpec {
        profile: "other".to_owned(),
        ..claude_resume_spec()
    };
    let other_kind = ResumeSpec {
        agent: AgentKind::Codex,
        ..claude_resume_spec()
    };
    for spec in [other_profile, other_kind] {
        let cmds = host.resume("s1", spec);
        assert_eq!(appends(&cmds), 0, "{cmds:?}");
        assert!(
            matches!(
                cmds.as_slice(),
                [HostCommand::Reply {
                    result: Err(WireError::InvalidParams(_)),
                    ..
                }]
            ),
            "{cmds:?}"
        );
    }
}

// ---------- input and resize ----------

#[test]
fn input_is_written_with_what_made_it() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    assert_eq!(
        host.handle(HostEvent::Input {
            session: sid("s1"),
            bytes: b"\x1b[<0;4;3M".to_vec(),
            kind: InputKind::Mouse,
        }),
        vec![HostCommand::WritePty {
            session: sid("s1"),
            bytes: b"\x1b[<0;4;3M".to_vec(),
            kind: InputKind::Mouse,
        }]
    );
}

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
            kind: InputKind::Keys,
        }),
        vec![HostCommand::WritePty {
            session: sid("s1"),
            bytes: b"ls\r".to_vec(),
            kind: InputKind::Keys,
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
                kind: InputKind::Keys,
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
    SelectWorkspace(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let id = 0u8..5;
    prop_oneof![
        id.clone().prop_map(Op::New),
        (id.clone(), 0u8..3).prop_map(|(i, n)| Op::Rename(i, n)),
        id.clone().prop_map(Op::Close),
        (id.clone(), proptest::option::of(-1i32..3)).prop_map(|(i, c)| Op::Exit(i, c)),
        id.clone().prop_map(Op::Input),
        id.clone().prop_map(Op::Resize),
        id.prop_map(Op::SelectWorkspace),
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
                    kind: InputKind::Keys,
                });
            }
            Op::Resize(i) => {
                host.handle(HostEvent::Resize {
                    session: sid(&id(i)),
                    size: PtySize { cols: 10, rows: 5 },
                });
            }
            Op::SelectWorkspace(i) => {
                select(&mut host, &format!("/src/w{i}"));
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

#[test]
fn sessions_are_ordered_by_instant_across_offsets_and_precisions() {
    // As strings these sort in exactly the reverse order.
    let mut host = HostDriver::new();
    host.pin_ts("2026-10-29T12:00:00Z");
    host.new_session("a");
    host.pin_ts("2026-10-29T12:00:00.500Z");
    host.new_session("b");
    host.pin_ts("2026-10-29T12:30:00+01:00");
    host.new_session("c");
    let ids: Vec<_> = host.host().sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [sid("b"), sid("a"), sid("c")]);
}

/// A valid RFC 3339 string for `secs.nanos` since the epoch, written at `offset_mins` from UTC with `digits`
/// fraction digits (the fraction is truncated, `0` writes none).
fn rfc3339(secs: i64, nanos: u32, digits: u32, offset_mins: i32) -> String {
    let instant = jiff::Timestamp::new(secs, 0).unwrap_or_else(|e| panic!("{e}"));
    let offset = jiff::tz::Offset::from_seconds(offset_mins * 60).unwrap_or_else(|e| panic!("{e}"));
    let dt = offset.to_datetime(instant);
    let fraction = match digits {
        0 => String::new(),
        d => format!(".{:0width$}", nanos / 10u32.pow(9 - d), width = d as usize),
    };
    let sign = if offset_mins < 0 { '-' } else { '+' };
    let abs = offset_mins.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}{sign}{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        abs / 60,
        abs % 60,
    )
}

fn created_ts() -> impl Strategy<Value = String> {
    // Within a few seconds of each other, so different offsets and precisions interleave.
    (
        1_793_000_000i64..1_793_000_004,
        0u32..1_000_000_000,
        0u32..=9,
        -720i32..=840,
    )
        .prop_map(|(secs, nanos, digits, offset)| rfc3339(secs, nanos, digits, offset))
}

proptest! {
    #[test]
    fn sessions_are_ordered_by_created_instant_then_id(stamps in proptest::collection::vec(created_ts(), 1..12)) {
        let mut host = HostDriver::new();
        for (i, ts) in stamps.iter().enumerate() {
            host.pin_ts(ts);
            host.new_session(&format!("s{i:02}"));
        }
        let mut expected: Vec<(jiff::Timestamp, String)> = stamps
            .iter()
            .enumerate()
            .map(|(i, ts)| (ts.parse::<jiff::Timestamp>().unwrap_or_else(|e| panic!("{e}")), format!("s{i:02}")))
            .collect();
        expected.sort();
        expected.reverse();
        let expected: Vec<String> = expected.into_iter().map(|(_, id)| id).collect();
        let actual: Vec<String> = host.host().sessions().into_iter().map(|s| s.id.to_string()).collect();
        prop_assert_eq!(actual, expected);
    }

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

// ---------- workspaces ----------

fn path(p: &str) -> PathBuf {
    PathBuf::from(p)
}

/// The draft the host appends to its own log for a `_shepherd/*` frame.
fn host_append(method: &str, params: Value) -> HostCommand {
    let frame = Frame::from_value(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
        .unwrap_or_else(|e| panic!("bad frame: {e}"));
    HostCommand::Append(Box::new(FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    )))
}

/// A user's select of `p`, its probe found to be the repo `p`. Returns the commands of the probe's result.
fn select(host: &mut HostDriver, p: &str) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let ask = WorkspaceAsk::Select(reply);
    let probe = host.handle(HostEvent::SelectWorkspace { ask, path: path(p) });
    assert_eq!(
        probe,
        vec![HostCommand::ProbeWorkspace { ask, path: path(p) }]
    );
    let cmds = host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(path(p)),
    });
    (reply, cmds)
}

#[test]
fn selecting_a_workspace_probes_it_then_appends_selected_to_the_host_log_and_acks() {
    let mut host = HostDriver::new();
    let (reply, cmds) = select(&mut host, "/src/app");
    assert_eq!(
        cmds,
        vec![
            host_append(
                "_shepherd/workspace/selected",
                json!({ "repo": "/src/app" })
            ),
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::Ack),
            },
        ]
    );
    assert_eq!(host.host().workspace(), Some(path("/src/app").as_path()));
}

#[test]
fn a_select_whose_probe_fails_replies_why_and_selects_nothing() {
    let mut host = HostDriver::new();
    select(&mut host, "/src/app");
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::WorkspaceProbed {
        ask: WorkspaceAsk::Select(reply),
        repo: Err("/tmp/x is not a repo".to_owned()),
    });
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Err(WireError::InvalidParams("/tmp/x is not a repo".to_owned())),
        }]
    );
    assert_eq!(host.host().workspace(), Some(path("/src/app").as_path()));
}

#[test]
fn selecting_the_current_workspace_again_acks_without_appending() {
    let mut host = HostDriver::new();
    select(&mut host, "/src/app");
    // The same path, spelled with a trailing slash: paths compare as paths.
    let (reply, cmds) = select(&mut host, "/src/app/");
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::Ack),
        }]
    );
}

#[test]
fn a_typed_path_starting_with_a_tilde_expands_to_the_home() {
    let home = path("/home/me");
    for (typed, expanded) in [
        ("~", "/home/me"),
        ("~/", "/home/me"),
        ("~/Code/x", "/home/me/Code/x"),
        ("~/../other", "/home/me/../other"),
    ] {
        assert_eq!(expand_home(&path(typed), &home), path(expanded), "{typed}");
    }
}

#[test]
fn a_typed_path_without_a_leading_tilde_is_left_alone() {
    let home = path("/home/me");
    for typed in ["/src/app", "app", "./app", "~other/app", "~x", "a/~/b", ""] {
        assert_eq!(expand_home(&path(typed), &home), path(typed), "{typed}");
    }
}

#[test]
fn workspaces_lists_each_one_used_once_most_recent_first_and_survives_replay() {
    let mut host = HostDriver::new();
    select(&mut host, "/src/a");
    select(&mut host, "/src/b");
    select(&mut host, "/src/a");
    select(&mut host, "/src/c");
    assert_eq!(
        host.host().workspaces(),
        [path("/src/c"), path("/src/a"), path("/src/b")]
    );
    assert_eq!(host.host().workspace(), Some(path("/src/c").as_path()));
    assert_eq!(replay(&host), *host.host());
}

#[test]
fn a_clients_cwd_is_probed_only_while_no_workspace_is_selected() {
    let mut host = HostDriver::new();
    let cwd = |host: &mut HostDriver| {
        host.handle(HostEvent::SelectWorkspace {
            ask: WorkspaceAsk::ClientCwd,
            path: path("/src/app/crates"),
        })
    };
    assert_eq!(
        cwd(&mut host),
        vec![HostCommand::ProbeWorkspace {
            ask: WorkspaceAsk::ClientCwd,
            path: path("/src/app/crates"),
        }]
    );
    select(&mut host, "/src/other");
    assert_eq!(cwd(&mut host), vec![]);
}

#[test]
fn a_clients_cwd_in_a_repo_is_selected_without_a_reply() {
    let mut host = HostDriver::new();
    let cmds = host.handle(HostEvent::WorkspaceProbed {
        ask: WorkspaceAsk::ClientCwd,
        repo: Ok(path("/src/app")),
    });
    assert_eq!(
        cmds,
        vec![host_append(
            "_shepherd/workspace/selected",
            json!({ "repo": "/src/app" })
        )]
    );
    assert_eq!(host.host().workspace(), Some(path("/src/app").as_path()));
}

#[test]
fn a_clients_cwd_never_replaces_a_selection_and_a_failed_probe_is_silent() {
    let mut host = HostDriver::new();
    let failed = host.handle(HostEvent::WorkspaceProbed {
        ask: WorkspaceAsk::ClientCwd,
        repo: Err("/home/me is not a repo".to_owned()),
    });
    assert_eq!(failed, vec![]);
    assert_eq!(host.host().workspace(), None);
    select(&mut host, "/src/app");
    let late = host.handle(HostEvent::WorkspaceProbed {
        ask: WorkspaceAsk::ClientCwd,
        repo: Ok(path("/src/other")),
    });
    assert_eq!(late, vec![]);
    assert_eq!(host.host().workspace(), Some(path("/src/app").as_path()));
}

#[test]
fn new_sessions_record_the_current_workspace() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    select(&mut host, "/src/app");
    let cmds = host.new_session("s2");
    assert_eq!(
        cmds[0],
        append(
            "s2",
            "_shepherd/session/created",
            // In a workspace it gets a worktree (T22): a branch now, its cwd once the worktree exists.
            json!({ "name": "shell-2", "workspace": "/src/app", "branch": "shell-2", "agent": "shell", "profile": "shell" })
        )
    );
    let workspaces: Vec<Option<PathBuf>> = host
        .host()
        .sessions()
        .into_iter()
        .map(|s| s.workspace)
        .collect();
    assert_eq!(workspaces, [Some(path("/src/app")), None]);
}

#[test]
fn a_probed_repo_whose_path_is_not_utf8_is_refused_and_selects_nothing() {
    use std::os::unix::ffi::OsStrExt;
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let repo = PathBuf::from(std::ffi::OsStr::from_bytes(b"/src/\xff"));
    let cmds = host.handle(HostEvent::WorkspaceProbed {
        ask: WorkspaceAsk::Select(reply),
        repo: Ok(repo),
    });
    assert_eq!(appends(&cmds), 0);
    assert!(
        matches!(
            cmds.as_slice(),
            [HostCommand::Reply {
                result: Err(WireError::InvalidParams(_)),
                ..
            }]
        ),
        "{cmds:?}"
    );
    assert_eq!(host.host().workspace(), None);
}

#[test]
fn probe_results_that_arrive_after_they_stopped_mattering_change_nothing() {
    // Probes run off the runtime thread, so a result can arrive after the selection it would have made stopped
    // mattering. Each row: what was selected meanwhile, the late result, and whether a user's select (answered) or a
    // client's cwd (never answered) asked for it.
    let cases: [(&str, &str, Result<&str, &str>, bool); 5] = [
        // Two clients said hello from different repos while nothing was selected; the first probe to finish won.
        ("another client's cwd won", "/src/a", Ok("/src/b"), false),
        (
            "the user selected meanwhile",
            "/src/app",
            Ok("/src/other"),
            false,
        ),
        (
            "a failed cwd probe after a selection",
            "/src/app",
            Err("/home/me is not a repo"),
            false,
        ),
        (
            "a cwd probe of the selected repo",
            "/src/app",
            Ok("/src/app"),
            false,
        ),
        // A user's select is still answered, but appends nothing for a repo someone else already selected.
        (
            "a user's select of the repo selected meanwhile",
            "/src/app",
            Ok("/src/app"),
            true,
        ),
    ];
    for (why, meanwhile, late, user) in cases {
        let mut host = HostDriver::new();
        select(&mut host, meanwhile);
        let before = host.host().clone();
        let ask = if user {
            WorkspaceAsk::Select(host.reply_to())
        } else {
            WorkspaceAsk::ClientCwd
        };
        let cmds = host.handle(HostEvent::WorkspaceProbed {
            ask,
            repo: late.map(path).map_err(str::to_owned),
        });
        let expected = match ask {
            WorkspaceAsk::Select(to) => vec![HostCommand::Reply {
                to,
                result: Ok(HostResult::Ack),
            }],
            WorkspaceAsk::ClientCwd => vec![],
        };
        assert_eq!(cmds, expected, "{why}");
        assert_eq!(*host.host(), before, "{why}");
    }
}
