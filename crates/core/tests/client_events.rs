//! The status page's events feed (T59): what happens in sessions across hosts, newest last, bounded.

use serde_json::{Value, json};
use shepherd_core::acpp::{Pr, PrChecks, PrState, PromptBy, Source, SourceKind};
use shepherd_core::client::events::{EVENTS_KEPT, EventKind};
use shepherd_core::client::{ClientEvent, Intent};
use shepherd_core::ids::SessionId;
use shepherd_testkit::builders::{ClientDriver, claude_session_spec, log_entry};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn hook(c: &mut ClientDriver, session: &str, method: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), source, frame);
    c.sync();
}

fn shepherd(c: &mut ClientDriver, session: &str, method: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    c.host().append(
        Some(session),
        Source::new(SourceKind::Shepherd, None),
        frame,
    );
    c.sync();
}

fn ask_permission(c: &mut ClientDriver, session: &str, title: &str) {
    let frame = json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
        "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": title }, "options": [] } });
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), source, frame);
    c.sync();
}

fn tool_update(c: &mut ClientDriver, session: &str) {
    hook(
        c,
        session,
        "session/update",
        json!({ "sessionId": "a1", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t1",
            "status": "in_progress" } }),
    );
}

/// A connected client whose host runs Claude Code session `id`, named `name`.
fn claude(id: &str, name: &str) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect();
    c.host().new_session_with(id, claude_session_spec());
    c.host().rename(id, name);
    c.sync();
    c
}

/// The feed as `(session, kind)`, oldest first.
fn feed(c: &ClientDriver) -> Vec<(String, EventKind)> {
    c.client()
        .events()
        .map(|e| (e.session.as_str().to_owned(), e.kind.clone()))
        .collect()
}

fn kinds(c: &ClientDriver) -> Vec<EventKind> {
    feed(c).into_iter().map(|(_, k)| k).collect()
}

#[test]
fn a_sessions_life_is_in_the_feed_in_order() {
    let mut c = claude("s1", "api");
    c.host().agent_started("s1", "claude-code", None);
    c.sync();
    hook(&mut c, "s1", "_shepherd/agent/idle", json!({}));
    shepherd(
        &mut c,
        "s1",
        "_shepherd/session/prompted",
        json!({ "text": "fix the login", "by": "mcp:s9" }),
    );
    c.host().user_prompt("s1", "fix the login");
    c.sync();
    ask_permission(&mut c, "s1", "Bash(cargo test)");
    tool_update(&mut c, "s1");
    hook(&mut c, "s1", "_shepherd/agent/idle", json!({}));
    shepherd(
        &mut c,
        "s1",
        "_shepherd/session/pr",
        json!({ "pr": { "number": 416, "state": "open", "checks": "failing" } }),
    );
    c.host().pty_exited("s1", Some(2));
    c.sync();
    assert_eq!(
        kinds(&c),
        [
            EventKind::Created,
            EventKind::Started {
                agent: "claude-code".to_owned()
            },
            EventKind::Prompted {
                text: "fix the login".to_owned(),
                by: PromptBy::Mcp(sid("s9")),
            },
            EventKind::Asked {
                text: Some("Bash(cargo test)".to_owned())
            },
            EventKind::Answered,
            EventKind::Finished,
            EventKind::Pr {
                pr: Some(Pr {
                    number: 416,
                    state: PrState::Open,
                    checks: PrChecks::Failing,
                })
            },
            EventKind::Exited { code: Some(2) },
        ]
    );
    let events: Vec<_> = c.client().events().collect();
    assert!(events.iter().all(|e| e.session == sid("s1")));
    assert_eq!(events[3].name, "api", "named as it was when it happened");
}

#[test]
fn a_closed_session_leaves_its_close_in_the_feed() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().rename("s1", "docs");
    c.host().close("s1");
    c.host().pty_exited("s1", Some(0));
    c.sync();
    assert!(c.client().sessions().is_empty());
    let last = c.client().events().last().cloned();
    let last = last.unwrap_or_else(|| panic!("no events"));
    assert_eq!(last.kind, EventKind::Closed);
    assert_eq!(last.name, "docs", "the name it had, though it is gone");
}

#[test]
fn a_subagent_waiting_on_the_user_is_an_ask() {
    let mut c = claude("s1", "api");
    c.host().user_prompt("s1", "go");
    c.sync();
    hook(
        &mut c,
        "s1",
        "_shepherd/subagent/started",
        json!({ "id": "a1", "agent_type": "builder" }),
    );
    hook(
        &mut c,
        "s1",
        "_shepherd/subagent/blocked",
        json!({ "id": "a1", "kind": "question", "text": "Which ALPN?" }),
    );
    assert_eq!(
        kinds(&c).last(),
        Some(&EventKind::Asked {
            text: Some("Which ALPN?".to_owned())
        })
    );
}

#[test]
fn an_asked_frame_with_its_permission_request_is_one_ask_then_answered() {
    // T51's shape (`_shepherd/agent/asked { kind, text, choices?, tool? }`, then `_shepherd/session/answered { choice?,
    // by }`) beside the hook's permission request: one ask, and its answer, however the fold reads the extra frames.
    let mut c = claude("s1", "api");
    c.host().user_prompt("s1", "go");
    c.sync();
    ask_permission(&mut c, "s1", "Bash(rm -rf target)");
    hook(
        &mut c,
        "s1",
        "_shepherd/agent/asked",
        json!({ "kind": "permission", "text": "Bash(rm -rf target)", "tool": "Bash" }),
    );
    shepherd(
        &mut c,
        "s1",
        "_shepherd/session/answered",
        json!({ "choice": "yes", "by": "client" }),
    );
    tool_update(&mut c, "s1");
    let asks: Vec<_> = kinds(&c)
        .into_iter()
        .filter(|k| matches!(k, EventKind::Asked { .. } | EventKind::Answered))
        .collect();
    assert_eq!(
        asks,
        [
            EventKind::Asked {
                text: Some("Bash(rm -rf target)".to_owned())
            },
            EventKind::Answered
        ]
    );
}

#[test]
fn events_are_ordered_by_when_they_happened_newest_last() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.host().pin_ts("2026-10-29T11:00:00.000Z");
    shepherd(&mut c, "s2", "_shepherd/session/pr", json!({}));
    c.host().unpin_ts();
    let feed = feed(&c);
    assert_eq!(
        feed.first().map(|(s, k)| (s.as_str(), k)),
        Some(("s2", &EventKind::Pr { pr: None })),
        "an hour before the sessions were made: {feed:?}"
    );
    let times: Vec<_> = c.client().events().map(|e| e.at).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
}

#[test]
fn the_feed_keeps_only_the_latest_events() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    for n in 0..EVENTS_KEPT as u64 + 5 {
        shepherd(
            &mut c,
            "s1",
            "_shepherd/session/pr",
            json!({ "pr": { "number": n, "state": "open", "checks": "none" } }),
        );
    }
    assert_eq!(c.client().events().len(), EVENTS_KEPT);
    let numbers: Vec<u64> = c
        .client()
        .events()
        .filter_map(|e| match &e.kind {
            EventKind::Pr { pr: Some(pr) } => Some(pr.number),
            _ => None,
        })
        .collect();
    assert_eq!(numbers.first(), Some(&5), "the oldest went first");
    assert_eq!(numbers.last(), Some(&(EVENTS_KEPT as u64 + 4)));
}

#[test]
fn an_entry_sent_again_is_not_a_second_event() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    shepherd(&mut c, "s1", "_shepherd/session/pr", json!({}));
    let before = c.client().events().len();
    let last = c.host().log().last().cloned();
    let last = last.unwrap_or_else(|| panic!("no entry"));
    c.handle(log_entry(last));
    assert_eq!(c.client().events().len(), before);
}

#[test]
fn an_event_is_as_old_as_the_latest_tick_says() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let at = c.client().events().last().map(|e| e.at);
    let at = at.unwrap_or_else(|| panic!("no events"));
    assert_eq!(
        c.client().events().last().and_then(|e| e.age(c.client())),
        None
    );
    let later = at
        .checked_add(jiff::SignedDuration::from_secs(90))
        .unwrap_or_else(|e| panic!("{e}"));
    c.handle(ClientEvent::Tick {
        at: shepherd_testkit::builders::ts(&later.to_string()),
    });
    let age = c.client().events().last().and_then(|e| e.age(c.client()));
    assert_eq!(age, Some(jiff::SignedDuration::from_secs(90)));
}

#[test]
fn a_status_select_from_the_feed_selects_its_listed_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    let first = c.client().events().next().map(|e| e.session.clone());
    let first = first.unwrap_or_else(|| panic!("no events"));
    assert_eq!(first, sid("s1"));
    c.user(Intent::StatusSelect {
        session: first,
        shown: 10,
    });
    assert_eq!(c.client().status_selected(), Some(&sid("s1")));
}
