//! Per-session hook translation (`shepherd_core::agents::Translators`), the state the host keeps between the hook
//! payloads `shepherd hook` sends it.

use serde_json::{Value, json};
use shepherd_core::acpp::{Frame, FrameDraft, RequestId};
use shepherd_core::agents::Translators;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn prompt() -> Value {
    json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": "hi" })
}

fn stop() -> Value {
    json!({ "hook_event_name": "Stop", "session_id": "c1" })
}

fn translate(t: &mut Translators, session: &str, payload: &Value) -> Vec<FrameDraft> {
    t.translate(&sid(session), AgentKind::ClaudeCode, payload, || {
        format!("seed-{session}")
    })
}

fn request_id(drafts: &[FrameDraft]) -> Option<RequestId> {
    drafts.iter().find_map(|d| match &d.frame {
        Frame::Acp(m) if m.raw().get("method").is_some() => m.id().cloned(),
        _ => None,
    })
}

fn response_id(drafts: &[FrameDraft]) -> Option<RequestId> {
    drafts.iter().find_map(|d| match &d.frame {
        Frame::Response(r) => Some(r.id().clone()),
        _ => None,
    })
}

#[test]
fn a_stop_answers_the_prompt_opened_by_an_earlier_hook_of_the_same_session() {
    let mut t = Translators::new();
    let opened = request_id(&translate(&mut t, "s1", &prompt())).expect("a prompt request");
    let answered = response_id(&translate(&mut t, "s1", &stop())).expect("a prompt response");
    assert_eq!(opened, answered);
}

#[test]
fn sessions_keep_separate_translation_state() {
    let mut t = Translators::new();
    translate(&mut t, "s1", &prompt());
    // s2 never opened a prompt, so its Stop has nothing to answer.
    assert_eq!(response_id(&translate(&mut t, "s2", &stop())), None);
    assert!(response_id(&translate(&mut t, "s1", &stop())).is_some());
}

#[test]
fn frames_belong_to_the_hooks_session() {
    let mut t = Translators::new();
    let drafts = translate(&mut t, "s7", &prompt());
    assert!(!drafts.is_empty());
    assert!(drafts.iter().all(|d| d.session == Some(sid("s7"))));
}

#[test]
fn the_seed_is_asked_once_per_session_and_prefixes_its_ids() {
    let mut t = Translators::new();
    let mut asked = 0;
    for _ in 0..2 {
        t.translate(&sid("s1"), AgentKind::ClaudeCode, &prompt(), || {
            asked += 1;
            "abc".to_owned()
        });
    }
    assert_eq!(asked, 1);
    let drafts = translate(&mut t, "s1", &prompt());
    let id = request_id(&drafts).expect("a prompt request");
    assert!(id.to_string().contains("abc"), "{id}");
}

#[test]
fn only_agent_kinds_with_a_translator_produce_frames() {
    assert!(Translators::supports(AgentKind::ClaudeCode));
    assert!(!Translators::supports(AgentKind::Shell));
    let mut t = Translators::new();
    assert!(
        t.translate(&sid("s1"), AgentKind::Shell, &prompt(), String::new)
            .is_empty()
    );
}

#[test]
fn removing_a_session_drops_its_translator_and_only_its() {
    let mut t = Translators::new();
    assert!(t.is_empty());
    translate(&mut t, "s_a", &prompt());
    translate(&mut t, "s_b", &prompt());
    assert_eq!(t.len(), 2);

    t.remove(&sid("s_a"));
    assert_eq!(t.len(), 1);
    // s_b keeps its state: its open prompt still pairs with its Stop.
    assert!(response_id(&translate(&mut t, "s_b", &stop())).is_some());
    // Removing a session without a translator does nothing.
    t.remove(&sid("s_nope"));
    assert_eq!(t.len(), 1);
}

#[test]
fn an_agent_kind_without_a_translator_leaves_no_state() {
    let mut t = Translators::new();
    t.translate(&sid("s1"), AgentKind::Shell, &prompt(), String::new);
    assert!(t.is_empty());
}
