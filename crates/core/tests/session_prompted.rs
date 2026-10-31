//! What the per-session fold knows about prompts (T47): whether the agent is at its prompt, and the last prompt the host
//! typed into it and who asked. Through the public `shepherd_core::session` API.

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, PromptBy, Seq, Source, SourceKind};
use shepherd_core::ids::SessionId;
use shepherd_core::session::{Activity, SessionState};
use shepherd_testkit::builders::EnvelopeBuilder;

fn shepherd(method: &str, params: Value) -> (Dir, Value) {
    (
        Dir::Internal,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn started() -> (Dir, Value) {
    shepherd(
        "_shepherd/agent/started",
        json!({ "agent": "claude-code", "agent_session_id": "abc" }),
    )
}

fn exited(code: i32) -> (Dir, Value) {
    shepherd("_shepherd/agent/exited", json!({ "code": code }))
}

fn prompted(text: &str, by: &str) -> (Dir, Value) {
    shepherd(
        "_shepherd/session/prompted",
        json!({ "text": text, "by": by }),
    )
}

fn prompt(id: i64, text: &str) -> (Dir, Value) {
    (
        Dir::ToAgent,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": text }] } }),
    )
}

fn prompt_done(id: i64) -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } }),
    )
}

/// Folds `steps` as a hook (Claude Code) wrote them.
fn fold(steps: &[(Dir, Value)]) -> SessionState {
    let mut log = EnvelopeBuilder::new();
    let mut state = SessionState::default();
    for (dir, frame) in steps {
        state.apply(&log.envelope(dir.clone(), frame.clone()));
    }
    state
}

#[test]
fn an_agent_that_reported_its_start_is_at_its_prompt_until_a_turn() {
    assert!(!fold(&[]).at_prompt(), "nothing started yet");
    assert!(fold(&[started()]).at_prompt());
    assert!(!fold(&[started(), prompt(1, "hi")]).at_prompt());
    assert!(fold(&[started(), prompt(1, "hi"), prompt_done(1)]).at_prompt());
    assert!(!fold(&[started(), exited(0)]).at_prompt());
}

#[test]
fn a_resume_the_host_started_is_not_at_its_prompt_until_the_agent_reports() {
    let mut host = EnvelopeBuilder::new().source(Source::new(SourceKind::Shepherd, None));
    let mut state = SessionState::default();
    let (dir, frame) = started();
    state.apply(&host.envelope(dir, frame));
    assert!(!state.at_prompt());
    // Then the agent's own `SessionStart`.
    let mut hook = EnvelopeBuilder::new();
    let (dir, frame) = started();
    state.apply(&hook.envelope(dir, frame));
    assert!(state.at_prompt());
}

#[test]
fn a_prompt_the_host_typed_starts_a_turn_and_is_remembered_with_who_asked() {
    let state = fold(&[started(), prompted("fix it\nplease", "mcp:s_pm")]);
    assert_eq!(state.activity(), &Activity::Working { tool: None });
    assert!(!state.at_prompt());
    let told = state.told().expect("told");
    assert_eq!(told.text, "fix it\nplease");
    assert_eq!(told.by, PromptBy::Mcp(SessionId::new("s_pm")));
    assert_eq!(told.seq, Seq::new(2));
    // The agent's own prompt and the end of its turn follow; what it was told stays.
    let state = fold(&[
        started(),
        prompted("fix it", "cli"),
        prompt(1, "fix it"),
        prompt_done(1),
    ]);
    assert!(state.at_prompt());
    assert_eq!(state.told().map(|t| t.text.as_str()), Some("fix it"));
    assert_eq!(state.told().map(|t| &t.by), Some(&PromptBy::Cli));
}

#[test]
fn a_subagent_waiting_on_the_user_keeps_the_agent_from_its_prompt_and_says_what_it_asked() {
    let blocked = shepherd(
        "_shepherd/subagent/blocked",
        json!({ "id": "a1", "kind": "question", "text": "Which branch?", "agent_type": "Explore" }),
    );
    let state = fold(&[started(), blocked]);
    assert!(!state.at_prompt());
    assert!(state.wants_user());
    assert_eq!(state.asked(), Some("Which branch?"));
    // The agent's own permission is what it asked, first.
    let permission = (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": "Bash" },
                            "options": [] } }),
    );
    let state = fold(&[started(), prompt(1, "go"), permission]);
    assert!(state.wants_user());
    assert_eq!(state.asked(), Some("Bash"));
    assert_eq!(fold(&[started()]).asked(), None);
}

#[test]
fn a_prompt_recorded_for_an_exited_agent_starts_no_turn() {
    let state = fold(&[started(), exited(1), prompted("hi", "cli")]);
    assert_eq!(state.activity(), &Activity::Exited { code: Some(1) });
}

#[test]
fn who_asked_reads_back_as_written_even_when_this_build_does_not_know_it() {
    for by in ["cli", "client", "mcp:s_1", "robot"] {
        let parsed: PromptBy = serde_json::from_value(json!(by)).expect("parses");
        assert_eq!(serde_json::to_value(&parsed).expect("writes"), json!(by));
    }
    let robot: PromptBy = serde_json::from_value(json!("robot")).expect("parses");
    assert_eq!(robot, PromptBy::Other("robot".to_owned()));
}
