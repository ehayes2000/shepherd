//! What a script drives the client with (T46): knowing when every host has sent its sessions, asking a host something
//! and reading its answer, each session's status, and naming a session (or a machine) by id or by name.

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::automation::{SessionStatus, Target, TargetError, resolve_machine};
use shepherd_core::client::{AskError, AskId, ClientCommand, ClientEvent, Intent};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    ClientRequest, HostResult, PROTOCOL_VERSION, SessionNew, SessionNewOk, WireError,
};
use shepherd_testkit::builders::{ClientDriver, claude_session_spec, requests, ts};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn hello_ok() -> serde_json::Value {
    json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "name": "studio" })
}

fn ask(id: u64, machine: Option<&str>, request: ClientRequest) -> ClientEvent {
    ClientEvent::Ask {
        ask: AskId(id),
        machine: machine.map(MachineId::new),
        request,
    }
}

fn new_session() -> ClientRequest {
    ClientRequest::SessionNew(SessionNew {
        profile: Some("shell".into()),
        ..SessionNew::default()
    })
}

/// A client connected and caught up with its host, which has no peers and sessions `ids` (the last newest).
fn settled_with(ids: &[&str]) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    let subscribe = c.answer(&hello, Ok(hello_ok()));
    for id in ids {
        c.host().new_session(id);
    }
    c.sync();
    let list = c.answer(&subscribe, Ok(json!({})));
    c.answer(&list, Ok(json!({ "peers": [] })));
    assert!(c.client().settled());
    c
}

fn agent_frame(c: &mut ClientDriver, session: &str, frame: serde_json::Value) {
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), hook, frame);
    c.sync();
}

// ---------- settled ----------

#[test]
fn the_client_is_settled_only_once_its_host_is_caught_up_and_has_listed_its_peers() {
    let mut c = ClientDriver::new();
    assert!(!c.client().settled(), "not started");
    c.handle(ClientEvent::Start);
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    assert!(!c.client().settled(), "hello unanswered");
    let subscribe = c.answer(&hello, Ok(hello_ok()));
    assert!(!c.client().settled(), "catching up");
    let list = c.answer(&subscribe, Ok(json!({})));
    assert!(!c.client().settled(), "peers not listed");
    c.answer(&list, Ok(json!({ "peers": [] })));
    assert!(c.client().settled());
}

#[test]
fn a_host_that_cannot_list_its_peers_still_settles_the_client() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    let subscribe = c.answer(&hello, Ok(hello_ok()));
    let list = c.answer(&subscribe, Ok(json!({})));
    c.answer(&list, Err(WireError::Internal("no".into())));
    assert!(c.client().settled());
}

#[test]
fn a_dropped_link_unsettles_the_client() {
    let mut c = settled_with(&[]);
    c.handle(ClientEvent::Disconnected);
    assert!(!c.client().settled());
}

// ---------- ask ----------

#[test]
fn an_ask_is_sent_to_the_local_host_and_its_answer_kept_for_the_caller() {
    let mut c = settled_with(&[]);
    let sent = c.handle(ask(7, None, new_session()));
    let [request] = requests(&sent)[..] else {
        panic!("one request in {sent:?}");
    };
    assert_eq!(request.body, new_session());
    assert_eq!(c.client().answer(AskId(7)), None, "not answered yet");
    c.answer(&sent, Ok(json!({ "session": "s9" })));
    assert_eq!(
        c.client().answer(AskId(7)),
        Some(&Ok(HostResult::SessionNew(SessionNewOk {
            session: sid("s9")
        })))
    );
}

#[test]
fn an_ask_naming_the_local_hosts_machine_goes_to_the_local_host() {
    let mut c = settled_with(&[]);
    let sent = c.handle(ask(1, Some("m_test"), new_session()));
    assert_eq!(requests(&sent).len(), 1, "{sent:?}");
}

#[test]
fn the_answer_to_an_ask_is_the_callers_and_the_client_does_not_act_on_it() {
    let mut c = settled_with(&["s1"]);
    c.user(Intent::SelectSession(sid("s1")));
    let sent = c.handle(ask(1, None, new_session()));
    c.host().new_session("s2");
    c.sync();
    c.answer(&sent, Ok(json!({ "session": "s2" })));
    // A session the user made is selected once listed; one a script made isn't.
    assert_eq!(c.client().selected(), Some(&sid("s1")));
}

#[test]
fn a_refused_ask_is_answered_with_the_hosts_reason() {
    let mut c = settled_with(&[]);
    let sent = c.handle(ask(1, None, new_session()));
    c.answer(
        &sent,
        Err(WireError::InvalidParams("unknown profile `nope`".into())),
    );
    let answer = c.client().answer(AskId(1)).cloned();
    assert_eq!(
        answer,
        Some(Err(AskError::Refused(WireError::InvalidParams(
            "unknown profile `nope`".into()
        ))))
    );
    let shown = answer.and_then(Result::err).map(|e| e.to_string());
    assert_eq!(shown.as_deref(), Some("unknown profile `nope`"));
    assert_eq!(c.client().notice(), None, "no notice for the user");
}

#[test]
fn an_ask_of_an_unknown_machine_is_answered_at_once_and_sends_nothing() {
    let mut c = settled_with(&[]);
    let sent = c.handle(ask(1, Some("m_nowhere"), new_session()));
    assert_eq!(sent, []);
    assert_eq!(
        c.client().answer(AskId(1)),
        Some(&Err(AskError::UnknownMachine(MachineId::new("m_nowhere"))))
    );
}

#[test]
fn an_ask_before_the_host_is_reached_is_answered_at_once_and_sends_nothing() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let sent = c.handle(ask(1, None, new_session()));
    assert_eq!(sent, []);
    assert!(
        matches!(
            c.client().answer(AskId(1)),
            Some(Err(AskError::NotReachable { .. }))
        ),
        "{:?}",
        c.client().answer(AskId(1))
    );
}

#[test]
fn an_ask_whose_link_drops_before_the_answer_is_answered_as_dropped() {
    let mut c = settled_with(&[]);
    c.handle(ask(1, None, new_session()));
    let reconnect = c.handle(ClientEvent::Disconnected);
    assert_eq!(reconnect, [ClientCommand::Connect]);
    assert!(
        matches!(
            c.client().answer(AskId(1)),
            Some(Err(AskError::Dropped { .. }))
        ),
        "{:?}",
        c.client().answer(AskId(1))
    );
}

// ---------- what a session is doing ----------

#[test]
fn a_session_lists_the_agent_it_runs() {
    let mut c = settled_with(&["s1"]);
    c.host().new_session_with("s2", claude_session_spec());
    c.sync();
    let agents: Vec<_> = c.client().sessions().iter().map(|s| s.agent).collect();
    assert_eq!(
        agents,
        [Some(AgentKind::ClaudeCode), Some(AgentKind::Shell)]
    );
}

#[test]
fn status_is_running_idle_wants_user_or_exited() {
    let mut c = settled_with(&["s1"]);
    let status = |c: &ClientDriver| SessionStatus::of(&c.client().sessions()[0]);
    assert_eq!(status(&c), SessionStatus::Running, "started, not idle yet");
    agent_frame(
        &mut c,
        "s1",
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    assert_eq!(status(&c), SessionStatus::Idle);
    agent_frame(
        &mut c,
        "s1",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": "go" }] } }),
    );
    assert_eq!(status(&c), SessionStatus::Running);
    agent_frame(
        &mut c,
        "s1",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": "rm" },
                            "options": [] } }),
    );
    assert_eq!(status(&c), SessionStatus::WantsUser);
    c.host().pty_exited("s1", Some(0));
    c.sync();
    assert_eq!(status(&c), SessionStatus::Exited);
}

#[test]
fn statuses_are_spelled_as_documented() {
    let spelled: Vec<&str> = [
        SessionStatus::Running,
        SessionStatus::Idle,
        SessionStatus::WantsUser,
        SessionStatus::Exited,
    ]
    .iter()
    .map(|s| s.as_str())
    .collect();
    assert_eq!(spelled, ["running", "idle", "wants-user", "exited"]);
}

// ---------- targets ----------

/// A settled client with sessions s1 "api", s2 "web", s3 "api" (renamed).
fn named() -> ClientDriver {
    let mut c = settled_with(&["s1", "s2", "s3"]);
    c.host().rename("s1", "api");
    c.host().rename("s2", "web");
    c.host().rename("s3", "api");
    c.sync();
    c
}

#[test]
fn a_target_names_a_session_by_id() {
    let c = named();
    let found = Target::new("s1").resolve(c.client());
    assert_eq!(found.map(|s| s.id.clone()), Ok(sid("s1")));
}

#[test]
fn a_target_names_a_session_by_its_unique_name() {
    let c = named();
    let found = Target::new("web").resolve(c.client());
    assert_eq!(found.map(|s| s.id.clone()), Ok(sid("s2")));
}

#[test]
fn an_id_wins_over_a_name() {
    let mut c = named();
    c.host().rename("s2", "s1");
    c.sync();
    let found = Target::new("s1").resolve(c.client());
    assert_eq!(found.map(|s| s.id.clone()), Ok(sid("s1")));
}

#[test]
fn an_ambiguous_name_is_an_error_listing_the_sessions_it_could_mean() {
    let c = named();
    let err = Target::new("api").resolve(c.client()).unwrap_err();
    let TargetError::Ambiguous { candidates, .. } = &err else {
        panic!("{err:?}");
    };
    let ids: Vec<_> = candidates.iter().map(|c| c.id.clone()).collect();
    assert_eq!(ids, [sid("s3"), sid("s1")], "newest first");
    let shown = err.to_string();
    assert!(
        shown.contains("s1") && shown.contains("s3") && !shown.contains("s2"),
        "{shown}"
    );
    assert!(shown.contains("studio"), "names the machine: {shown}");
}

#[test]
fn an_unknown_target_is_an_error_listing_every_session() {
    let c = named();
    let err = Target::new("nope").resolve(c.client()).unwrap_err();
    let TargetError::Unknown { candidates, .. } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(candidates.len(), 3);
    let shown = err.to_string();
    assert!(shown.contains("nope") && shown.contains("web"), "{shown}");
}

#[test]
fn an_unknown_target_with_no_sessions_says_there_are_none() {
    let c = settled_with(&[]);
    let err = Target::new("x").resolve(c.client()).unwrap_err();
    assert!(err.to_string().contains("no sessions"), "{err}");
}

#[test]
fn a_machine_is_named_by_id_or_name() {
    let c = settled_with(&[]);
    for text in ["m_test", "studio"] {
        let found = resolve_machine(c.client(), text).map(|m| m.id);
        assert_eq!(found, Ok(MachineId::new("m_test")), "{text}");
    }
    let err = resolve_machine(c.client(), "laptop").unwrap_err();
    assert!(
        err.to_string().contains("laptop") && err.to_string().contains("studio"),
        "{err}"
    );
}
