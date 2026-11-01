//! Subagents in the client's sidebar (T55): each session carries its live subagents, the tree under a session is
//! expanded per window, and a blocked subagent makes its session look blocked.

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::{Intent, SidebarEntry};
use shepherd_core::ids::SessionId;
use shepherd_core::session::SubagentStatus;
use shepherd_testkit::builders::ClientDriver;

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn hook() -> Source {
    Source::new(SourceKind::Hook, Some("claude-code".to_owned()))
}

fn frame(driver: &mut ClientDriver, session: &str, method: &str, params: serde_json::Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    driver.host().append(Some(session), hook(), frame);
    driver.sync();
}

fn started(driver: &mut ClientDriver, session: &str, id: &str, agent_type: &str, task: &str) {
    let params = json!({ "id": id, "agent_type": agent_type, "task": task });
    frame(driver, session, "_shepherd/subagent/started", params);
}

fn blocked(driver: &mut ClientDriver, session: &str, id: &str) {
    let params = json!({ "id": id, "kind": "permission", "text": "Bash(ls)" });
    frame(driver, session, "_shepherd/subagent/blocked", params);
}

fn idle(driver: &mut ClientDriver, session: &str) {
    frame(driver, session, "_shepherd/agent/idle", json!({}));
}

/// A connected client with sessions `ids` (the last newest, selected), each prompted so it is working.
fn working(ids: &[&str]) -> ClientDriver {
    let mut driver = ClientDriver::with_sessions(ids);
    for id in ids {
        driver.host().user_prompt(id, "go");
    }
    driver.sync();
    driver
}

/// The sidebar as text: a session's id, or `  <type>` (`  └<type>` for the last) for a subagent row.
fn rows(driver: &ClientDriver) -> Vec<String> {
    driver
        .client()
        .sidebar()
        .into_iter()
        .map(|e| match e {
            SidebarEntry::Machine(g) => format!("# {}", g.name),
            SidebarEntry::Session(s) => s.id.as_str().to_owned(),
            SidebarEntry::Subagent {
                parent,
                subagent,
                last,
            } => format!(
                "  {}{}{}",
                parent.as_str(),
                if last { "└" } else { "├" },
                subagent.agent_type()
            ),
        })
        .collect()
}

fn session<'a>(driver: &'a ClientDriver, id: &str) -> &'a shepherd_core::client::ClientSession {
    driver
        .client()
        .sessions()
        .iter()
        .find(|s| s.id.as_str() == id)
        .unwrap_or_else(|| panic!("{id} is not listed"))
}

#[test]
fn a_session_carries_its_subagents_in_the_order_they_started() {
    let mut driver = working(&["s1"]);
    started(&mut driver, "s1", "a1", "builder", "T28");
    started(&mut driver, "s1", "a2", "reviewer", "F36");
    let types: Vec<&str> = session(&driver, "s1")
        .subagents
        .iter()
        .map(|s| s.agent_type())
        .collect();
    assert_eq!(types, ["builder", "reviewer"]);
}

#[test]
fn the_tree_under_a_session_is_collapsed_until_toggled() {
    let mut driver = working(&["s1"]);
    started(&mut driver, "s1", "a1", "builder", "T28");
    assert!(!driver.client().expanded(&sid("s1")));
    assert_eq!(rows(&driver), ["s1"]);
}

#[test]
fn toggling_a_session_lists_its_subagents_under_it_the_last_marked() {
    let mut driver = working(&["s1", "s2"]);
    started(&mut driver, "s1", "a1", "builder", "T28");
    started(&mut driver, "s1", "a2", "reviewer", "F36");
    let commands = driver.user(Intent::ToggleSubagents(sid("s1")));
    assert!(
        commands.is_empty(),
        "the tree is the window's own: {commands:?}"
    );
    assert!(driver.client().expanded(&sid("s1")));
    assert_eq!(rows(&driver), ["s2", "s1", "  s1├builder", "  s1└reviewer"]);
}

#[test]
fn toggling_again_collapses_the_tree() {
    let mut driver = working(&["s1"]);
    started(&mut driver, "s1", "a1", "builder", "T28");
    driver.user(Intent::ToggleSubagents(sid("s1")));
    driver.user(Intent::ToggleSubagents(sid("s1")));
    assert!(!driver.client().expanded(&sid("s1")));
    assert_eq!(rows(&driver), ["s1"]);
}

#[test]
fn a_session_without_subagents_does_not_toggle() {
    let mut driver = working(&["s1"]);
    driver.user(Intent::ToggleSubagents(sid("s1")));
    assert!(!driver.client().expanded(&sid("s1")));
    driver.user(Intent::ToggleSubagents(sid("nope")));
    assert!(!driver.client().expanded(&sid("nope")));
}

#[test]
fn finished_subagents_stay_listed_until_the_turn_ends() {
    let mut driver = working(&["s1"]);
    started(&mut driver, "s1", "a1", "Explore", "callers");
    frame(
        &mut driver,
        "s1",
        "_shepherd/subagent/finished",
        json!({ "id": "a1" }),
    );
    driver.user(Intent::ToggleSubagents(sid("s1")));
    let subs = &session(&driver, "s1").subagents;
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].status(), &SubagentStatus::Finished);
    idle(&mut driver, "s1");
    assert!(session(&driver, "s1").subagents.is_empty());
    assert_eq!(rows(&driver), ["s1"]);
}

#[test]
fn a_blocked_subagent_makes_its_session_blocked_on_the_user() {
    let mut driver = working(&["s1"]);
    started(&mut driver, "s1", "a1", "reviewer", "F36");
    assert!(!session(&driver, "s1").subagent_blocked());
    blocked(&mut driver, "s1", "a1");
    assert!(session(&driver, "s1").subagent_blocked());
}

#[test]
fn expanding_a_tree_above_the_selection_keeps_the_selection_in_view() {
    // s1 (oldest) is at the bottom; s3 sits above it.
    let mut driver = working(&["s1", "s2", "s3"]);
    driver.user(Intent::SelectSession(sid("s1")));
    driver.user(Intent::SidebarRows(3));
    for i in 0..6 {
        started(&mut driver, "s2", &format!("a{i}"), "builder", "t");
    }
    driver.user(Intent::ToggleSubagents(sid("s2")));
    let selected = driver
        .client()
        .sidebar()
        .iter()
        .position(|e| matches!(e, SidebarEntry::Session(s) if s.id == sid("s1")))
        .unwrap_or_else(|| panic!("s1 is listed"));
    let first = driver.client().sidebar_scroll();
    assert!(
        (first..first + 3).contains(&selected),
        "s1 at {selected}, shown from {first}: {:?}",
        rows(&driver)
    );
}

#[test]
fn the_wheel_scrolls_through_the_tree() {
    let mut driver = working(&["s1"]);
    driver.user(Intent::SidebarRows(2));
    for i in 0..5 {
        started(&mut driver, "s1", &format!("a{i}"), "builder", "t");
    }
    driver.user(Intent::ToggleSubagents(sid("s1")));
    driver.user(Intent::ScrollSidebar(10));
    // Six entries, two shown at once: the last two.
    assert_eq!(driver.client().sidebar_scroll(), 4);
}

#[test]
fn a_closed_session_forgets_its_tree() {
    let mut driver = working(&["s1", "s2"]);
    started(&mut driver, "s1", "a1", "builder", "T28");
    driver.user(Intent::ToggleSubagents(sid("s1")));
    driver.host().close("s1");
    driver.sync();
    assert!(!driver.client().expanded(&sid("s1")));
}
