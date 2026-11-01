//! Answering in place from the status page's preview (T58): the selected session's pending question, the buttons the
//! host would accept for it, `_shepherd/session/answer` sent with the question's seq and `by: client`, and how the
//! answer went.

use std::num::NonZeroU32;

use serde_json::{Value, json};
use shepherd_core::acpp::{Choice, PromptBy, Source, SourceKind};
use shepherd_core::client::answer::{AnswerOutcome, AnswerRefusal, Answers, Unanswerable};
use shepherd_core::client::{ClientCommand, ClientEvent, Intent};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{ClientRequest, SessionAnswer, Status, WireError};
use shepherd_testkit::builders::{ClientDriver, claude_session_spec, requests, shell_session_spec};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn hook(c: &mut ClientDriver, session: &str, frame: Value) {
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), source, frame);
    c.sync();
}

/// A connected client whose host runs Claude Code sessions `ids` (oldest first), each in a turn.
fn agents(ids: &[&str]) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect();
    for id in ids {
        c.host().new_session_with(id, claude_session_spec());
        c.host().rename(id, id);
        c.host().user_prompt(id, "work");
    }
    c.sync();
    c
}

/// `session`'s agent asks permission for `tool` (`Bash(cargo test)`), and says what it asks (`agent/asked`).
fn asks_permission(c: &mut ClientDriver, session: &str, tool: &str) {
    hook(
        c,
        session,
        json!({ "jsonrpc": "2.0", "id": 9, "method": "session/request_permission",
            "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t9", "title": format!("{tool}(cargo test)") },
                        "options": [] } }),
    );
    hook(
        c,
        session,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/asked",
            "params": { "kind": "permission", "text": format!("{tool}: cargo test"), "tool": tool } }),
    );
}

/// `session`'s agent asks a question with `choices`.
fn asks_question(c: &mut ClientDriver, session: &str, choices: &[&str]) {
    hook(
        c,
        session,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/asked",
            "params": { "kind": "question", "text": "Which ALPN?", "choices": choices,
                        "tool": "AskUserQuestion" } }),
    );
}

fn select(c: &mut ClientDriver, session: &str) {
    c.user(Intent::StatusSelect {
        session: sid(session),
        shown: 10,
    });
}

/// The seq of the question `session`'s agent is blocked on.
fn asked_seq(c: &ClientDriver, session: &str) -> shepherd_core::acpp::Seq {
    c.client()
        .state(&sid(session))
        .and_then(|s| s.question_seq())
        .unwrap_or_else(|| panic!("a pending question"))
}

fn answers_sent(commands: &[ClientCommand]) -> Vec<SessionAnswer> {
    requests(commands)
        .into_iter()
        .filter_map(|r| match &r.body {
            ClientRequest::SessionAnswer(a) => Some(a.clone()),
            _ => None,
        })
        .collect()
}

fn answer(c: &mut ClientDriver, session: &str, choice: Choice) -> Vec<ClientCommand> {
    let asked = asked_seq(c, session);
    c.user(Intent::StatusAnswer {
        session: sid(session),
        asked,
        choice,
    })
}

/// The selected session's buttons as `(choice, label, refused)`.
fn buttons(c: &ClientDriver) -> Vec<(Choice, String, bool)> {
    match c.client().status_asked().map(|a| a.answers) {
        Some(Answers::Open { buttons, .. }) => buttons
            .iter()
            .map(|b| (b.choice, b.label.to_owned(), b.refused.is_some()))
            .collect(),
        other => panic!("not answerable: {other:?}"),
    }
}

fn option(n: u32) -> Choice {
    Choice::Option(NonZeroU32::new(n).unwrap_or_else(|| panic!("from 1")))
}

fn outcome(c: &ClientDriver) -> Option<AnswerOutcome> {
    c.client().answer_report().map(|r| r.outcome.clone())
}

#[test]
fn the_selected_sessions_question_is_shown_only_while_it_is_pending() {
    let mut c = agents(&["s1", "s2"]);
    select(&mut c, "s1");
    assert!(c.client().status_asked().is_none(), "working, not asking");
    asks_permission(&mut c, "s1", "Edit");
    let asked = c.client().status_asked().unwrap_or_else(|| panic!("asked"));
    assert_eq!(asked.session, &sid("s1"));
    assert_eq!(asked.text, Some("Edit(cargo test)"));
    assert!(matches!(asked.answers, Answers::Open { .. }));
    // Another session's question isn't the selected one's.
    asks_permission(&mut c, "s2", "Bash");
    assert_eq!(
        c.client().status_asked().map(|a| a.session.clone()),
        Some(sid("s1"))
    );
    select(&mut c, "s2");
    assert_eq!(
        c.client().status_asked().map(|a| a.session.clone()),
        Some(sid("s2"))
    );
}

#[test]
fn a_claude_file_prompt_offers_yes_always_and_no() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    assert_eq!(
        buttons(&c),
        [
            (Choice::Yes, "yes".to_owned(), false),
            (Choice::Always, "always".to_owned(), false),
            (Choice::No, "no".to_owned(), false),
        ]
    );
}

#[test]
fn always_is_refused_where_the_host_would_refuse_it_and_says_why() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Bash");
    assert_eq!(
        buttons(&c),
        [
            (Choice::Yes, "yes".to_owned(), false),
            (Choice::Always, "always".to_owned(), true),
            (Choice::No, "no".to_owned(), false),
        ]
    );
    let Some(Answers::Open { buttons, .. }) = c.client().status_asked().map(|a| a.answers) else {
        panic!("not answerable");
    };
    let why = buttons[1].refused.clone().unwrap_or_default();
    assert!(why.contains("doesn't offer always"), "{why}");
    // Pressing it anyway sends nothing, and says why.
    let sent = answer(&mut c, "s1", Choice::Always);
    assert!(answers_sent(&sent).is_empty());
    assert!(matches!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::NotOffered(why))) if why.contains("doesn't offer always")
    ));
}

#[test]
fn a_question_offers_its_options_and_no() {
    let mut c = agents(&["s1"]);
    asks_question(&mut c, "s1", &["h3", "shepherd/1"]);
    assert_eq!(
        buttons(&c),
        [
            (option(1), "h3".to_owned(), false),
            (option(2), "shepherd/1".to_owned(), false),
            (Choice::No, "no".to_owned(), false),
        ]
    );
    let sent = answer(&mut c, "s1", option(2));
    assert_eq!(answers_sent(&sent)[0].choice, option(2));
}

#[test]
fn answering_sends_the_choice_with_the_questions_seq_by_client() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let asked = asked_seq(&c, "s1");
    let sent = answer(&mut c, "s1", Choice::Yes);
    assert_eq!(
        answers_sent(&sent),
        [SessionAnswer {
            session: sid("s1"),
            choice: Choice::Yes,
            asked: Some(asked),
            by: Some(PromptBy::Client),
        }]
    );
    assert_eq!(outcome(&c), Some(AnswerOutcome::Sending));
    // A second press while the first is unanswered sends nothing more.
    assert!(answers_sent(&answer(&mut c, "s1", Choice::No)).is_empty());
    c.answer(&sent, Ok(json!({})));
    assert_eq!(outcome(&c), Some(AnswerOutcome::Answered));
}

#[test]
fn an_answer_the_host_refuses_as_already_answered_says_so() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let sent = answer(&mut c, "s1", Choice::Yes);
    c.answer(
        &sent,
        Err(WireError::NothingAsked {
            session: sid("s1"),
            status: Status::Running,
        }),
    );
    assert_eq!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::AlreadyAnswered))
    );
}

#[test]
fn a_stale_answer_for_a_question_no_longer_pending_is_refused_without_asking() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let stale = asked_seq(&c, "s1");
    // The user answered in the pane, and the agent moved on.
    hook(
        &mut c,
        "s1",
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "a1",
            "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t9", "status": "in_progress" } } }),
    );
    assert!(c.client().status_asked().is_none());
    let sent = c.user(Intent::StatusAnswer {
        session: sid("s1"),
        asked: stale,
        choice: Choice::Yes,
    });
    assert!(answers_sent(&sent).is_empty());
    assert_eq!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::AlreadyAnswered))
    );
}

#[test]
fn a_question_typed_into_in_the_pane_is_shown_without_buttons() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    c.host().append(
        Some("s1"),
        Source::new(SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/answered", "params": { "by": "client" } }),
    );
    c.sync();
    let asked = c
        .client()
        .status_asked()
        .unwrap_or_else(|| panic!("still asked"));
    assert_eq!(asked.text, Some("Edit(cargo test)"));
    assert!(matches!(
        asked.answers,
        Answers::Unanswerable(Unanswerable::InPane)
    ));
}

#[test]
fn a_dropped_link_refuses_an_unanswered_answer() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    answer(&mut c, "s1", Choice::Yes);
    c.handle(ClientEvent::Disconnected);
    assert!(matches!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::Other(why))) if why.contains("dropped")
    ));
}

/// The user answers `session`'s open question in the pane, its turn ends, and it starts another.
fn moves_on(c: &mut ClientDriver, session: &str) {
    hook(
        c,
        session,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    c.host().user_prompt(session, "next");
    c.sync();
}

#[test]
fn an_answer_to_an_earlier_question_never_lands_on_the_next() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let first = asked_seq(&c, "s1");
    moves_on(&mut c, "s1");
    asks_question(&mut c, "s1", &["h3", "shepherd/1"]);
    let second = asked_seq(&c, "s1");
    assert_ne!(first, second);
    assert!(matches!(
        c.client().status_asked().map(|a| a.answers),
        Some(Answers::Open { asked, .. }) if asked == second
    ));
    let sent = c.user(Intent::StatusAnswer {
        session: sid("s1"),
        asked: first,
        choice: Choice::No,
    });
    assert!(answers_sent(&sent).is_empty());
    assert_eq!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::AlreadyAnswered))
    );
}

#[test]
fn a_question_on_a_host_that_cannot_be_reached_is_unanswerable() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let asked = asked_seq(&c, "s1");
    c.handle(ClientEvent::Disconnected);
    assert!(matches!(
        c.client().status_asked().map(|a| a.answers),
        Some(Answers::Unanswerable(Unanswerable::Offline))
    ));
    let sent = c.user(Intent::StatusAnswer {
        session: sid("s1"),
        asked,
        choice: Choice::Yes,
    });
    assert!(answers_sent(&sent).is_empty());
    assert!(matches!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::Other(why))) if why.contains("can't be reached")
    ));
}

#[test]
fn an_answer_racing_the_user_typing_in_the_pane_says_so() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    let asked = asked_seq(&c, "s1");
    c.host().append(
        Some("s1"),
        Source::new(SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/answered", "params": { "by": "client" } }),
    );
    c.sync();
    let sent = c.user(Intent::StatusAnswer {
        session: sid("s1"),
        asked,
        choice: Choice::Yes,
    });
    assert!(answers_sent(&sent).is_empty());
    assert_eq!(
        outcome(&c),
        Some(AnswerOutcome::Refused(AnswerRefusal::InPane))
    );
}

#[test]
fn an_exited_sessions_question_is_not_shown() {
    let mut c = agents(&["s1"]);
    asks_permission(&mut c, "s1", "Edit");
    c.host().pty_exited("s1", Some(1));
    c.sync();
    assert!(c.client().status_asked().is_none());
}

#[test]
fn a_session_running_no_agent_offers_no_buttons() {
    let mut c = ClientDriver::new();
    c.connect();
    c.host().new_session_with("sh", shell_session_spec());
    c.host().user_prompt("sh", "work");
    c.sync();
    asks_permission(&mut c, "sh", "Edit");
    assert!(matches!(
        c.client().status_asked().map(|a| a.answers),
        Some(Answers::Unanswerable(Unanswerable::InPane))
    ));
}
