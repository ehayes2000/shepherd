//! Prompting an agent and reading its reply (T47, decision 0022): the host types a prompt into an agent only while it
//! is at its prompt, records it, and reads replies from the transcript its hooks named. Through the public
//! `shepherd_core::host` API.

use std::path::PathBuf;

use serde_json::json;
use shepherd_core::acpp::{Frame, PromptBy, ShepherdFrame, Source, SourceKind};
use shepherd_core::config::AgentKind;
use shepherd_core::host::prompt::ReplyJob;
use shepherd_core::host::{HostCommand, HostEvent, ReplyTo};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Reply, TranscriptError, TurnMarker};
use shepherd_core::wire::{Busy, HostResult, InputKind, ReplyInfo, SessionReplyOk, WireError};
use shepherd_testkit::builders::{HostDriver, claude_session_spec};

const TRANSCRIPT: &str = "/home/test/.claude/projects/-src-app/abc.jsonl";

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// Claude Code session `s1`, running in its own cwd, whose agent reported its start (and transcript): at its prompt.
fn ready() -> HostDriver {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started_with_transcript("s1", "claude-code", "abc", TRANSCRIPT);
    host
}

fn prompt(host: &mut HostDriver, text: &str) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::Prompt {
        reply,
        session: sid("s1"),
        text: text.to_owned(),
        by: PromptBy::Cli,
    });
    (reply, commands)
}

fn written(commands: &[HostCommand]) -> Vec<(Vec<u8>, InputKind)> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::WritePty { bytes, kind, .. } => Some((bytes.clone(), *kind)),
            _ => None,
        })
        .collect()
}

fn answer(commands: &[HostCommand], to: ReplyTo) -> Option<&Result<HostResult, WireError>> {
    commands.iter().find_map(|c| match c {
        HostCommand::Reply { to: t, result } if *t == to => Some(result),
        _ => None,
    })
}

fn prompted(host: &HostDriver) -> Vec<(String, PromptBy)> {
    host.log()
        .iter()
        .filter_map(|e| match e.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::SessionPrompted(p) => Some((p.text.clone(), p.by.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn busy(status: Busy) -> Result<HostResult, WireError> {
    Err(WireError::SessionBusy {
        session: sid("s1"),
        status,
    })
}

#[test]
fn a_prompt_is_pasted_whole_then_submitted_and_recorded_with_who_asked() {
    let mut host = ready();
    let (reply, commands) = prompt(&mut host, "first line\nsecond line");
    assert_eq!(
        written(&commands),
        [
            (
                b"\x1b[200~first line\nsecond line\x1b[201~".to_vec(),
                InputKind::Paste
            ),
            (b"\r".to_vec(), InputKind::Keys),
        ]
    );
    assert_eq!(answer(&commands, reply), Some(&Ok(HostResult::Ack)));
    assert_eq!(
        prompted(&host),
        [("first line\nsecond line".to_owned(), PromptBy::Cli)]
    );
    // The record comes before the typing, so the turn is the session's before the agent sees a key.
    let record = commands
        .iter()
        .position(|c| matches!(c, HostCommand::Append(_)));
    let typing = commands
        .iter()
        .position(|c| matches!(c, HostCommand::WritePty { .. }));
    assert!(record < typing, "{commands:?}");
}

#[test]
fn what_would_end_the_paste_early_or_that_the_agent_would_strip_is_never_typed() {
    let mut host = ready();
    // C1's CSI (U+009B) would start a sequence too; DEL is a key; a tab stays.
    let text =
        "a\x1b[201~\rrm -rf\u{7}\r\nb\u{202e}c\u{200b}d\u{9b}2~\u{7f}\t \u{1F469}\u{200D}\u{1F4BB}";
    let (_, commands) = prompt(&mut host, text);
    let typed = &written(&commands)[0].0;
    assert_eq!(
        String::from_utf8_lossy(typed),
        "\x1b[200~a[201~\nrm -rf\nbcd2~\t \u{1F469}\u{200D}\u{1F4BB}\x1b[201~"
    );
    assert_eq!(
        prompted(&host)[0].0,
        "a[201~\nrm -rf\nbcd2~\t \u{1F469}\u{200D}\u{1F4BB}"
    );
}

#[test]
fn a_second_prompt_before_the_turn_ends_is_refused_as_busy_and_typed_nowhere() {
    let mut host = ready();
    prompt(&mut host, "one");
    let (reply, commands) = prompt(&mut host, "two");
    assert_eq!(answer(&commands, reply), Some(&busy(Busy::Running)));
    assert!(written(&commands).is_empty());
    assert_eq!(prompted(&host).len(), 1);
}

#[test]
fn an_agent_in_a_turn_or_waiting_on_the_user_is_refused_with_its_status() {
    let mut host = ready();
    host.prompt_frame("s1", "the user's own", false);
    let (reply, commands) = prompt(&mut host, "mine");
    assert_eq!(answer(&commands, reply), Some(&busy(Busy::Running)));

    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    host.append(
        Some("s1"),
        hook,
        json!({ "jsonrpc": "2.0", "id": "perm1", "method": "session/request_permission",
                "params": { "sessionId": "c1", "toolCall": { "toolCallId": "t1", "title": "Bash" },
                            "options": [] } }),
    );
    let (reply, commands) = prompt(&mut host, "mine");
    assert_eq!(answer(&commands, reply), Some(&busy(Busy::WantsUser)));
    assert!(written(&commands).is_empty());
}

#[test]
fn an_agent_whose_subagent_waits_on_the_user_is_refused_as_wanting_the_user() {
    let mut host = ready();
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    host.append(
        Some("s1"),
        hook,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/blocked",
                "params": { "id": "a1", "kind": "question", "text": "Which branch?",
                            "agent_type": "Explore" } }),
    );
    let (reply, commands) = prompt(&mut host, "mine");
    assert_eq!(answer(&commands, reply), Some(&busy(Busy::WantsUser)));
    assert!(written(&commands).is_empty());
}

#[test]
fn an_agent_that_has_not_reported_its_start_is_still_starting() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    let (reply, commands) = prompt(&mut host, "hi");
    assert_eq!(answer(&commands, reply), Some(&busy(Busy::Running)));
}

#[test]
fn an_idle_agent_takes_a_prompt_after_its_turn_ends() {
    let mut host = ready();
    prompt(&mut host, "one");
    host.prompt_frame("s1", "one", false);
    let idle = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    host.append(
        Some("s1"),
        idle,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    let (reply, commands) = prompt(&mut host, "two");
    assert_eq!(answer(&commands, reply), Some(&Ok(HostResult::Ack)));
    assert_eq!(written(&commands).len(), 2);
}

#[test]
fn a_shell_an_exited_session_or_an_unknown_one_is_refused() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let (reply, commands) = prompt(&mut host, "ls");
    assert!(
        matches!(answer(&commands, reply), Some(Err(WireError::InvalidParams(why))) if why.contains("no agent")),
        "{commands:?}"
    );
    assert!(written(&commands).is_empty());

    let mut host = ready();
    host.pty_exited("s1", Some(0));
    let (reply, commands) = prompt(&mut host, "hi");
    assert!(
        matches!(
            answer(&commands, reply),
            Some(Err(WireError::InvalidParams(_)))
        ),
        "{commands:?}"
    );

    let mut host = HostDriver::new();
    let (reply, commands) = prompt(&mut host, "hi");
    assert_eq!(
        answer(&commands, reply),
        Some(&Err(WireError::UnknownSession { session: sid("s1") }))
    );
}

#[test]
fn a_prompt_with_nothing_left_to_type_is_refused() {
    let mut host = ready();
    let (reply, commands) = prompt(&mut host, "\u{7}\u{200b}");
    assert!(
        matches!(
            answer(&commands, reply),
            Some(Err(WireError::InvalidParams(_)))
        ),
        "{commands:?}"
    );
    assert!(written(&commands).is_empty());
}

// ---------- replies ----------

fn read_reply(host: &mut HostDriver, since: Option<TurnMarker>) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::ReadReply {
        reply,
        session: sid("s1"),
        since,
    });
    (reply, commands)
}

#[test]
fn a_reply_is_read_from_the_transcript_the_hooks_named() {
    let mut host = ready();
    let since = TurnMarker {
        at: "2026-10-31T10:00:00Z"
            .parse()
            .unwrap_or_else(|e| panic!("{e}")),
    };
    let (reply, commands) = read_reply(&mut host, Some(since));
    let job = commands.iter().find_map(|c| match c {
        HostCommand::ReadReply { reply: r, job } if *r == reply => Some(job),
        _ => None,
    });
    assert_eq!(
        job,
        Some(&ReplyJob {
            agent: AgentKind::ClaudeCode,
            transcript_path: PathBuf::from(TRANSCRIPT),
            since: Some(since),
            profile: Some("claude".to_owned()),
            cwd: PathBuf::from("/home/test"),
        })
    );
    // What the transcript gave is the answer.
    let read = Reply {
        text: "Done.".into(),
        at: None,
    };
    let commands = host.handle(HostEvent::ReplyRead {
        reply,
        result: Ok(Some(read)),
    });
    assert_eq!(
        answer(&commands, reply),
        Some(&Ok(HostResult::SessionReply(SessionReplyOk {
            reply: Some(ReplyInfo {
                text: "Done.".into(),
                at: None
            })
        })))
    );
    // A transcript never written holds no reply; one that can't be read is an error.
    let commands = host.handle(HostEvent::ReplyRead {
        reply,
        result: Err(TranscriptError::Missing("gone".into())),
    });
    assert_eq!(
        answer(&commands, reply),
        Some(&Ok(HostResult::SessionReply(SessionReplyOk {
            reply: None
        })))
    );
    let commands = host.handle(HostEvent::ReplyRead {
        reply,
        result: Err(TranscriptError::Refused("a symlink".into())),
    });
    assert!(matches!(
        answer(&commands, reply),
        Some(Err(WireError::InvalidParams(_)))
    ));
}

#[test]
fn an_agent_whose_hooks_named_no_transcript_has_no_reply_yet() {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    let (reply, commands) = read_reply(&mut host, None);
    assert_eq!(
        answer(&commands, reply),
        Some(&Ok(HostResult::SessionReply(SessionReplyOk {
            reply: None
        })))
    );
}

#[test]
fn a_shell_has_no_reply_to_read() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let (reply, commands) = read_reply(&mut host, None);
    assert!(
        matches!(answer(&commands, reply), Some(Err(WireError::InvalidParams(why))) if why.contains("no agent")),
        "{commands:?}"
    );
}
