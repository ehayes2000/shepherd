//! The status page's message line (T59): prompt the selected agent, or every eligible agent of a group, `by: client`,
//! and report who was refused and why.

use serde_json::json;
use shepherd_core::acpp::{PromptBy, Source, SourceKind};
use shepherd_core::client::message::{MessageTarget, Outcome, Refusal, Skip};
use shepherd_core::client::{ClientCommand, ClientEvent, Intent};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{Busy, ClientRequest, SessionPrompt, WireError};
use shepherd_testkit::builders::{ClientDriver, claude_session_spec, requests};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn idle(c: &mut ClientDriver, session: &str) {
    let frame = json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} });
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), source, frame);
    c.sync();
}

/// A connected client whose host runs Claude Code sessions `ids` (oldest first), each named after its id, and a
/// shell session `sh` (newest).
fn agents(ids: &[&str]) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect();
    for id in ids {
        c.host().new_session_with(id, claude_session_spec());
        c.host().rename(id, id);
    }
    c.host().new_session("sh");
    c.host().rename("sh", "sh");
    c.sync();
    c
}

fn prompts(commands: &[ClientCommand]) -> Vec<SessionPrompt> {
    requests(commands)
        .into_iter()
        .filter_map(|r| match &r.body {
            ClientRequest::SessionPrompt(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// Types `text` into the message line and sends it.
fn send(c: &mut ClientDriver, text: &str) -> Vec<ClientCommand> {
    c.user(Intent::ComposeMessage(true));
    c.user(Intent::EditMessage(text.to_owned()));
    c.user(Intent::SendMessage)
}

/// Answers the prompt to `session` among `commands` with `result`.
fn answer(
    c: &mut ClientDriver,
    commands: &[ClientCommand],
    session: &str,
    result: Result<serde_json::Value, WireError>,
) {
    let request = requests(commands)
        .into_iter()
        .find(
            |r| matches!(&r.body, ClientRequest::SessionPrompt(p) if p.session.as_str() == session),
        )
        .unwrap_or_else(|| panic!("no prompt to {session} in {commands:?}"));
    let one = vec![ClientCommand::Send(
        shepherd_core::wire::ClientMessage::Request(request.clone()),
    )];
    c.answer(&one, result);
}

/// The last report as `(session, outcome)`.
fn report(c: &ClientDriver) -> Vec<(String, Outcome)> {
    c.client()
        .message_report()
        .map(|r| {
            r.deliveries
                .iter()
                .map(|d| (d.session.as_str().to_owned(), d.outcome.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_message_line_prompts_the_selected_agent_by_client() {
    let mut c = agents(&["a1", "a2"]);
    idle(&mut c, "a1");
    c.user(Intent::StatusSelect {
        session: sid("a1"),
        shown: 10,
    });
    assert_eq!(c.client().message_target(), &MessageTarget::Selected);
    let sent = send(&mut c, "fix the login");
    assert_eq!(
        prompts(&sent),
        [SessionPrompt {
            session: sid("a1"),
            text: "fix the login".to_owned(),
            by: Some(PromptBy::Client),
        }]
    );
    assert_eq!(c.client().message_text(), "", "sent text is cleared");
    assert_eq!(report(&c), [("a1".to_owned(), Outcome::Sending)]);
    answer(&mut c, &sent, "a1", Ok(json!({})));
    assert_eq!(report(&c), [("a1".to_owned(), Outcome::Sent)]);
}

#[test]
fn editing_and_composing_are_the_lines_own() {
    let mut c = agents(&["a1"]);
    assert!(!c.client().composing());
    c.user(Intent::ComposeMessage(true));
    assert!(c.client().composing());
    c.user(Intent::EditMessage("hi".to_owned()));
    assert_eq!(c.client().message_text(), "hi");
    c.user(Intent::ComposeMessage(false));
    assert!(!c.client().composing());
    assert_eq!(c.client().message_text(), "hi", "kept for later");
}

#[test]
fn blank_text_sends_nothing() {
    let mut c = agents(&["a1"]);
    idle(&mut c, "a1");
    assert!(prompts(&send(&mut c, "   ")).is_empty());
    assert!(c.client().message_report().is_none());
}

#[test]
fn tab_widens_the_target_to_all_working_all_idle_then_each_machine_and_back() {
    let mut c = agents(&["a1"]);
    let mut seen = vec![c.client().message_target().clone()];
    for _ in 0..4 {
        let next = c.client().next_message_target();
        c.user(Intent::MessageTarget(next));
        seen.push(c.client().message_target().clone());
    }
    assert_eq!(
        seen,
        [
            MessageTarget::Selected,
            MessageTarget::Working,
            MessageTarget::Idle,
            MessageTarget::Machine(MachineId::new("m_test")),
            MessageTarget::Selected,
        ]
    );
}

#[test]
fn a_group_send_prompts_each_eligible_agent_and_reports_who_refused_and_why() {
    let mut c = agents(&["a1", "a2", "a3"]);
    for id in ["a1", "a2", "a3"] {
        idle(&mut c, id);
    }
    c.user(Intent::MessageTarget(MessageTarget::Idle));
    let sent = send(&mut c, "rebase on main");
    let to: Vec<String> = prompts(&sent)
        .iter()
        .map(|p| p.session.as_str().to_owned())
        .collect();
    assert_eq!(to.len(), 3, "each idle agent: {to:?}");
    assert!(
        prompts(&sent)
            .iter()
            .all(|p| p.by == Some(PromptBy::Client))
    );
    answer(&mut c, &sent, "a1", Ok(json!({})));
    let busy = |status| WireError::SessionBusy {
        session: sid("x"),
        status,
    };
    answer(&mut c, &sent, "a2", Err(busy(Busy::WantsUser)));
    answer(&mut c, &sent, "a3", Err(busy(Busy::Running)));
    let mut got = report(&c);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            ("a1".to_owned(), Outcome::Sent),
            (
                "a2".to_owned(),
                Outcome::Refused(Refusal::Busy(Busy::WantsUser))
            ),
            (
                "a3".to_owned(),
                Outcome::Refused(Refusal::Busy(Busy::Running))
            ),
        ]
    );
    assert!(c.client().message_report().is_some_and(|r| r.done()));
}

#[test]
fn a_machine_send_skips_sessions_that_cant_be_prompted() {
    let mut c = agents(&["a1", "gone"]);
    idle(&mut c, "a1");
    c.host().pty_exited("gone", Some(0));
    c.sync();
    c.user(Intent::MessageTarget(MessageTarget::Machine(
        MachineId::new("m_test"),
    )));
    let sent = send(&mut c, "status?");
    let to: Vec<SessionId> = prompts(&sent).into_iter().map(|p| p.session).collect();
    assert_eq!(to, [sid("a1")]);
    let mut got = report(&c);
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        [
            ("a1".to_owned(), Outcome::Sending),
            ("gone".to_owned(), Outcome::Skipped(Skip::Exited)),
            ("sh".to_owned(), Outcome::Skipped(Skip::NoAgent)),
        ]
    );
}

#[test]
fn any_other_refusal_says_the_hosts_reason() {
    let mut c = agents(&["a1"]);
    idle(&mut c, "a1");
    c.user(Intent::StatusSelect {
        session: sid("a1"),
        shown: 10,
    });
    let sent = send(&mut c, "go");
    answer(
        &mut c,
        &sent,
        "a1",
        Err(WireError::InvalidParams("a1 isn't running".to_owned())),
    );
    assert_eq!(
        report(&c),
        [(
            "a1".to_owned(),
            Outcome::Refused(Refusal::Other("a1 isn't running".to_owned()))
        )]
    );
    assert_eq!(
        c.client().notice(),
        None,
        "the report says it, not a notice"
    );
}

#[test]
fn a_dropped_link_refuses_what_it_hadnt_answered() {
    let mut c = agents(&["a1"]);
    idle(&mut c, "a1");
    c.user(Intent::StatusSelect {
        session: sid("a1"),
        shown: 10,
    });
    send(&mut c, "go");
    c.handle(ClientEvent::Disconnected);
    let got = report(&c);
    assert!(
        matches!(&got[..], [(_, Outcome::Refused(Refusal::Other(why)))] if why.contains("dropped")),
        "{got:?}"
    );
}

#[test]
fn the_target_names_who_it_reaches() {
    let mut c = agents(&["a1", "a2"]);
    idle(&mut c, "a1");
    c.user(Intent::MessageTarget(MessageTarget::Idle));
    assert_eq!(c.client().message_recipients(), [sid("a1")]);
    c.user(Intent::MessageTarget(MessageTarget::Working));
    assert_eq!(c.client().message_recipients(), [sid("sh"), sid("a2")]);
}
