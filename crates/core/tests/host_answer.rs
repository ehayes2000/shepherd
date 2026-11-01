//! Answering an agent's pending question (T51): the host types the agent's own keys for a choice only while the
//! question it is blocked on is pending, records the answer, and refuses (typing nothing) with the session's status
//! otherwise, including once the user has answered in the pane. Through the public `shepherd_core::host` API.

use std::num::NonZeroU32;

use serde_json::{Value, json};
use shepherd_core::acpp::{Choice, Frame, PromptBy, SessionAnswered, ShepherdFrame};
use shepherd_core::agents::Translators;
use shepherd_core::config::AgentKind;
use shepherd_core::host::{HostCommand, HostEvent, ReplyTo};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::SpawnSpec;
use shepherd_core::wire::{HostResult, InputKind, Status, WireError};
use shepherd_testkit::builders::{
    HostDriver, claude_session_spec, shell_session_spec, shell_spawn,
};

macro_rules! cc {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/claude/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

macro_rules! cx {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/codex/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// Session `s1` running `agent`, fed hook payloads through the real translators as the runtime does.
struct Agent {
    host: HostDriver,
    agent: AgentKind,
    translators: Translators,
}

impl Agent {
    fn new(agent: AgentKind) -> Self {
        let mut host = HostDriver::new();
        let spec = match agent {
            AgentKind::Codex => {
                let mut spec = claude_session_spec();
                spec.profile = "codex".to_owned();
                spec.agent = AgentKind::Codex;
                spec.spawn = SpawnSpec {
                    command: "codex".to_owned(),
                    ..shell_spawn()
                };
                spec
            }
            _ => claude_session_spec(),
        };
        host.new_session_with("s1", spec);
        Self {
            host,
            agent,
            translators: Translators::new(),
        }
    }

    fn claude() -> Self {
        Self::new(AgentKind::ClaudeCode)
    }

    fn codex() -> Self {
        Self::new(AgentKind::Codex)
    }

    fn hooks(&mut self, payloads: Vec<Value>) -> &mut Self {
        for payload in payloads {
            let drafts = self
                .translators
                .translate(&sid("s1"), self.agent, &payload, || "seed".to_owned());
            for draft in drafts {
                self.host.append_draft(draft);
            }
        }
        self
    }

    fn answer(&mut self, choice: Choice) -> (ReplyTo, Vec<HostCommand>) {
        let reply = self.host.reply_to();
        let commands = self.host.handle(HostEvent::Answer {
            reply,
            session: sid("s1"),
            choice,
            by: PromptBy::Cli,
        });
        (reply, commands)
    }

    fn input(&mut self, bytes: &[u8], kind: InputKind) -> Vec<HostCommand> {
        self.host.handle(HostEvent::Input {
            session: sid("s1"),
            bytes: bytes.to_vec(),
            kind,
        })
    }

    fn answers(&self) -> Vec<SessionAnswered> {
        self.host
            .log()
            .iter()
            .filter_map(|e| match e.frame() {
                Frame::Shepherd(m) => match m.body() {
                    ShepherdFrame::SessionAnswered(a) => Some(a.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

fn written(commands: &[HostCommand]) -> Vec<Vec<u8>> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::WritePty { session, bytes, .. } if *session == sid("s1") => {
                Some(bytes.clone())
            }
            _ => None,
        })
        .collect()
}

fn result(commands: &[HostCommand], to: ReplyTo) -> Option<&Result<HostResult, WireError>> {
    commands.iter().find_map(|c| match c {
        HostCommand::Reply { to: t, result } if *t == to => Some(result),
        _ => None,
    })
}

fn nothing_asked(status: Status) -> Result<HostResult, WireError> {
    Err(WireError::NothingAsked {
        session: sid("s1"),
        status,
    })
}

fn option(n: u32) -> Choice {
    Choice::Option(NonZeroU32::new(n).unwrap_or_else(|| panic!("option {n} counts from 1")))
}

fn claude_permission() -> Agent {
    let mut agent = Agent::claude();
    agent.hooks(vec![
        cc!("SessionStart.startup"),
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
    ]);
    agent
}

fn claude_question() -> Agent {
    let mut agent = Agent::claude();
    agent.hooks(vec![
        cc!("SessionStart.startup"),
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
    ]);
    agent
}

fn codex_permission() -> Agent {
    let mut agent = Agent::codex();
    agent.hooks(vec![
        cx!("SessionStart.startup"),
        cx!("UserPromptSubmit"),
        cx!("PreToolUse.Bash"),
        cx!("PermissionRequest.Bash"),
    ]);
    agent
}

#[test]
fn a_claude_permission_is_answered_with_its_dialogs_own_keys() {
    for (choice, keys) in [
        (Choice::Yes, b"\r".to_vec()),
        (Choice::Always, b"\x1b[Z".to_vec()),
        (Choice::No, b"\x1b".to_vec()),
    ] {
        let mut agent = claude_permission();
        let (reply, commands) = agent.answer(choice);
        assert_eq!(
            result(&commands, reply),
            Some(&Ok(HostResult::Ack)),
            "{choice}"
        );
        assert_eq!(written(&commands), [keys], "{choice}");
        assert_eq!(
            agent.answers(),
            [SessionAnswered {
                choice: Some(choice),
                by: PromptBy::Cli
            }]
        );
        // The record comes before the keys, so the question is gone before the agent sees a key.
        let record = commands
            .iter()
            .position(|c| matches!(c, HostCommand::Append(_)));
        let typing = commands
            .iter()
            .position(|c| matches!(c, HostCommand::WritePty { .. }));
        assert!(record < typing, "{commands:?}");
    }
}

#[test]
fn a_codex_approval_is_answered_with_its_overlays_own_keys() {
    for (choice, keys) in [
        (Choice::Yes, b"y".to_vec()),
        (Choice::Always, b"a".to_vec()),
        (Choice::No, b"n".to_vec()),
    ] {
        let mut agent = codex_permission();
        let (reply, commands) = agent.answer(choice);
        assert_eq!(
            result(&commands, reply),
            Some(&Ok(HostResult::Ack)),
            "{choice}"
        );
        assert_eq!(written(&commands), [keys], "{choice}");
    }
}

#[test]
fn a_question_is_answered_by_moving_to_its_option_then_enter() {
    let mut agent = claude_question();
    let (reply, commands) = agent.answer(option(2));
    assert_eq!(result(&commands, reply), Some(&Ok(HostResult::Ack)));
    assert_eq!(written(&commands), [b"\x1b[B".to_vec(), b"\r".to_vec()]);

    let mut agent = claude_question();
    let (_, commands) = agent.answer(option(1));
    assert_eq!(written(&commands), [b"\r".to_vec()]);

    // `no` cancels the question.
    let mut agent = claude_question();
    let (_, commands) = agent.answer(Choice::No);
    assert_eq!(written(&commands), [b"\x1b".to_vec()]);
}

#[test]
fn a_choice_the_question_does_not_take_is_refused_and_typed_nowhere() {
    for (mut agent, choice) in [
        (claude_question(), option(3)),
        (claude_question(), Choice::Yes),
        (claude_question(), Choice::Always),
        (claude_permission(), option(1)),
        (codex_permission(), option(1)),
    ] {
        let (reply, commands) = agent.answer(choice);
        assert!(
            matches!(
                result(&commands, reply),
                Some(Err(WireError::InvalidParams(_)))
            ),
            "{choice}: {commands:?}"
        );
        assert!(written(&commands).is_empty());
        assert!(agent.answers().is_empty());
    }
}

#[test]
fn a_question_with_more_than_one_pick_cannot_be_answered_by_index() {
    let mut ask = cc!("PreToolUse.AskUserQuestion");
    ask["tool_input"]["questions"][0]["multiSelect"] = json!(true);
    let mut agent = Agent::claude();
    agent.hooks(vec![cc!("UserPromptSubmit"), ask]);
    let (reply, commands) = agent.answer(option(1));
    assert!(matches!(
        result(&commands, reply),
        Some(Err(WireError::InvalidParams(_)))
    ));
    assert!(written(&commands).is_empty());
}

#[test]
fn asked_then_answered_then_working() {
    let mut agent = claude_permission();
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(result(&commands, reply), Some(&Ok(HostResult::Ack)));
    // A second answer finds nothing pending: the agent works on.
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Running))
    );
    assert!(written(&commands).is_empty());
    // The agent's own activity follows; nothing more to answer.
    agent.hooks(vec![cc!("PostToolUse.Bash")]);
    let (reply, commands) = agent.answer(Choice::No);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Running))
    );
}

#[test]
fn an_answer_with_nothing_pending_is_refused_with_the_sessions_status() {
    let mut agent = Agent::claude();
    agent.hooks(vec![cc!("SessionStart.startup")]);
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(result(&commands, reply), Some(&nothing_asked(Status::Idle)));
    assert!(written(&commands).is_empty());

    agent.hooks(vec![cc!("UserPromptSubmit")]);
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Running))
    );

    agent.host.pty_exited("s1", Some(0));
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Exited))
    );
    assert!(written(&commands).is_empty());
    assert!(agent.answers().is_empty());
}

#[test]
fn a_stale_answer_after_the_user_answered_in_the_pane_is_refused_never_typed() {
    // The user pressed Enter in the dialog; no hook has said so yet.
    let mut agent = claude_permission();
    let commands = agent.input(b"\r", InputKind::Answer);
    assert_eq!(written(&commands), [b"\r".to_vec()]);
    assert_eq!(
        agent.answers(),
        [SessionAnswered {
            choice: None,
            by: PromptBy::Client
        }]
    );
    let (reply, commands) = agent.answer(Choice::No);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::WantsUser))
    );
    assert!(written(&commands).is_empty());

    // Moving around the dialog counts too: Enter would confirm what the user moved to.
    let mut agent = claude_permission();
    agent.input(b"\x1b[B", InputKind::Keys);
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::WantsUser))
    );
    assert!(written(&commands).is_empty());

    // Once the hooks say the agent moved on, it is refused as running.
    let mut agent = claude_permission();
    agent.input(b"\r", InputKind::Answer);
    agent.hooks(vec![cc!("PostToolUse.Bash")]);
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Running))
    );
}

#[test]
fn typing_in_the_pane_is_recorded_once_and_focus_reports_are_not_typing() {
    let mut agent = claude_permission();
    agent.input(b"\x1b[I", InputKind::Focus);
    assert!(agent.answers().is_empty());
    agent.input(b"\x1b[B", InputKind::Keys);
    agent.input(b"\r", InputKind::Answer);
    assert_eq!(agent.answers().len(), 1);
    // With nothing asked, typing records nothing.
    let mut agent = Agent::claude();
    agent.hooks(vec![cc!("SessionStart.startup")]);
    agent.input(b"hi", InputKind::Keys);
    assert!(agent.answers().is_empty());
}

#[test]
fn a_subagents_permission_is_answered_in_the_parent_sessions_pty() {
    let mut agent = Agent::claude();
    agent.hooks(vec![
        cc!("SessionStart.startup"),
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(result(&commands, reply), Some(&Ok(HostResult::Ack)));
    assert_eq!(written(&commands), [b"\r".to_vec()]);
    // The subagent works on, so the session no longer waits on the user.
    let (reply, commands) = agent.answer(Choice::Yes);
    assert_eq!(
        result(&commands, reply),
        Some(&nothing_asked(Status::Running))
    );
    assert!(written(&commands).is_empty());
}

#[test]
fn a_session_without_an_agent_or_unknown_is_refused() {
    let mut host = HostDriver::new();
    host.new_session_with("sh", shell_session_spec());
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::Answer {
        reply,
        session: sid("sh"),
        choice: Choice::Yes,
        by: PromptBy::Cli,
    });
    assert!(matches!(
        result(&commands, reply),
        Some(Err(WireError::InvalidParams(_)))
    ));
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::Answer {
        reply,
        session: sid("nope"),
        choice: Choice::Yes,
        by: PromptBy::Cli,
    });
    assert!(matches!(
        result(&commands, reply),
        Some(Err(WireError::UnknownSession { .. }))
    ));
}
