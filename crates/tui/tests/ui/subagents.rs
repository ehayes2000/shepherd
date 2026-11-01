//! A session's subagents in the sidebar (T55): the live count as a badge on its row, and the tree under it one click
//! away.

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use serde_json::{Value, json};
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::Intent;
use shepherd_core::ids::SessionId;
use shepherd_tui::Target;
use shepherd_tui::layout::layout;

use crate::harness::Ui;

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// A frame on session `session`'s log, as a hook translates it.
fn frame(ui: &mut Ui, session: &str, method: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    ui.on_host(|h| {
        h.append(Some(session), source, frame);
    });
}

fn started(ui: &mut Ui, session: &str, id: &str, agent_type: &str, task: &str) {
    let params = json!({ "id": id, "agent_type": agent_type, "task": task });
    frame(ui, session, "_shepherd/subagent/started", params);
}

fn blocked(ui: &mut Ui, session: &str, id: &str) {
    let params = json!({ "id": id, "kind": "permission", "text": "Bash(git diff)" });
    frame(ui, session, "_shepherd/subagent/blocked", params);
}

fn finished(ui: &mut Ui, session: &str, id: &str) {
    frame(
        ui,
        session,
        "_shepherd/subagent/finished",
        json!({ "id": id }),
    );
}

fn turn_ends(ui: &mut Ui, session: &str) {
    frame(ui, session, "_shepherd/agent/idle", json!({}));
}

/// Sessions `pm` (s1, working, selected) and `docs` (s2, working) in a `cols` x `rows` window.
fn working(cols: u16, rows: u16) -> Ui {
    let mut ui = Ui::with_sessions(&["docs", "pm"], cols, rows);
    ui.on_host(|h| {
        h.user_prompt("s1", "write the docs");
        h.user_prompt("s2", "dispatch the tasks");
        h.rename("s1", "docs");
        h.rename("s2", "pm");
    });
    ui
}

/// `pm` with four subagents: two builders working, a reviewer and an Explore that finished.
fn pm_with_a_team(cols: u16, rows: u16) -> Ui {
    let mut ui = working(cols, rows);
    started(&mut ui, "s2", "a1", "builder", "T28 hello build");
    started(&mut ui, "s2", "a2", "builder", "F38+F40 search");
    started(&mut ui, "s2", "a3", "reviewer", "F36 cleanup");
    started(&mut ui, "s2", "a4", "Explore", "find callers");
    finished(&mut ui, "s2", "a4");
    ui
}

/// The sidebar's area.
fn sidebar(ui: &Ui) -> Rect {
    layout(ui.window()).sidebar
}

#[test]
fn sidebar_with_a_collapsed_tree() {
    let ui = pm_with_a_team(80, 14);
    ui.assert_contains("▸4");
    ui.assert_lacks("builder");
    insta::assert_snapshot!(ui.region(sidebar(&ui)));
}

#[test]
fn sidebar_with_an_expanded_tree() {
    let mut ui = pm_with_a_team(80, 14);
    ui.click("▸4");
    ui.assert_contains("▾4");
    insta::assert_snapshot!(ui.region(sidebar(&ui)));
}

#[test]
fn a_session_without_subagents_has_no_badge() {
    let ui = working(80, 14);
    ui.assert_lacks("▸").assert_lacks("▾");
}

#[test]
fn clicking_the_badge_again_collapses_the_tree() {
    let mut ui = pm_with_a_team(80, 14);
    ui.click("▸4").click("▾4");
    ui.assert_contains("▸4").assert_lacks("builder");
}

#[test]
fn the_badge_toggles_without_changing_the_selection() {
    let mut ui = pm_with_a_team(80, 14);
    ui.click("docs");
    ui.click("▸4");
    assert_eq!(ui.client().selected(), Some(&sid("s1")));
    ui.assert_contains("▾4");
}

#[test]
fn tab_after_the_prefix_toggles_the_selected_sessions_tree() {
    let mut ui = pm_with_a_team(80, 14);
    ui.ctrl('b').key(KeyCode::Tab);
    ui.assert_contains("▾4");
    assert_eq!(ui.client().focus(), shepherd_core::client::Focus::Terminal);
    ui.ctrl('b').key(KeyCode::Tab);
    ui.assert_contains("▸4");
}

#[test]
fn the_badge_and_the_tree_rows_are_click_targets() {
    let mut ui = pm_with_a_team(80, 14);
    let badge = ui.at("▸4");
    assert_eq!(
        ui.hit_at(badge.x, badge.y),
        Some(Target::Subagents(sid("s2")))
    );
    ui.click("▸4");
    let row = ui.at("reviewer");
    assert_eq!(
        ui.hit_at(row.x, row.y),
        Some(Target::Subagent {
            session: sid("s2"),
            id: "a3".to_owned()
        })
    );
    // A subagent's row selects its session.
    ui.click("docs");
    ui.click("reviewer");
    assert_eq!(ui.client().selected(), Some(&sid("s2")));
}

#[test]
fn a_blocked_subagent_turns_its_sessions_glyph_yellow_collapsed_or_not() {
    let mut ui = pm_with_a_team(80, 14);
    blocked(&mut ui, "s2", "a3");
    let glyph_at = |ui: &Ui| {
        let name = ui.at("pm");
        (name.x - 2, name.y)
    };
    let (x, y) = glyph_at(&ui);
    assert_eq!(ui.buffer()[(x, y)].symbol(), "▲");
    assert_eq!(ui.buffer()[(x, y)].fg, Color::Yellow);
    // The badge turns yellow too, from cyan.
    let badge = ui.at("▸4");
    assert_eq!(ui.buffer()[(badge.x, badge.y)].fg, Color::Yellow);
    insta::assert_snapshot!("a_blocked_subagent_collapsed", ui.region(sidebar(&ui)));
    ui.click("▸4");
    let (x, y) = glyph_at(&ui);
    assert_eq!(ui.buffer()[(x, y)].symbol(), "▲");
    insta::assert_snapshot!("a_blocked_subagent_expanded", ui.region(sidebar(&ui)));
}

#[test]
fn a_badge_without_a_blocked_subagent_is_cyan() {
    let ui = pm_with_a_team(80, 14);
    let badge = ui.at("▸4");
    assert_eq!(ui.buffer()[(badge.x, badge.y)].fg, Color::Cyan);
}

#[test]
fn in_the_narrow_sidebar_the_glyph_and_the_branch_toggle_the_tree() {
    let mut ui = pm_with_a_team(40, 14);
    // pm is the top session: its glyph is the first `●` in the glyph column.
    let glyph = ui.find_in(sidebar(&ui), "●")[0];
    assert_eq!(
        ui.hit_at(glyph.x, glyph.y),
        Some(Target::Subagents(sid("s2")))
    );
    ui.click_at(glyph.x, glyph.y);
    assert!(ui.client().expanded(&sid("s2")));
    let branch = ui.at_in(sidebar(&ui), "└");
    assert_eq!(
        ui.hit_at(branch.x, branch.y),
        Some(Target::Subagents(sid("s2")))
    );
    ui.click_at(branch.x, branch.y);
    assert!(!ui.client().expanded(&sid("s2")));
    // The row's other cells still select it.
    ui.user(Intent::SelectSession(sid("s1")));
    ui.click_at(glyph.x, glyph.y + 1);
    assert_eq!(ui.client().selected(), Some(&sid("s2")));
}

#[test]
fn a_finished_subagent_is_dimmed_until_the_turn_ends_then_gone() {
    let mut ui = pm_with_a_team(80, 14);
    ui.click("▸4");
    let explore = ui.at("Explore");
    assert!(
        ui.buffer()[(explore.x, explore.y)]
            .modifier
            .contains(Modifier::DIM)
    );
    let builder = ui.find("builder")[0];
    assert!(
        !ui.buffer()[(builder.x, builder.y)]
            .modifier
            .contains(Modifier::DIM)
    );
    turn_ends(&mut ui, "s2");
    ui.assert_lacks("Explore")
        .assert_lacks("builder")
        .assert_lacks("▾")
        .assert_lacks("▸");
}

#[test]
fn a_tree_taller_than_the_sidebar_keeps_the_selection_visible() {
    let mut ui = Ui::with_sessions(&["one", "two", "three"], 80, 12);
    ui.on_host(|h| {
        for id in ["s1", "s2", "s3"] {
            h.user_prompt(id, "go");
        }
        h.rename("s1", "one");
        h.rename("s2", "two");
        h.rename("s3", "three");
    });
    for i in 0..12 {
        started(
            &mut ui,
            "s2",
            &format!("a{i}"),
            "builder",
            &format!("task {i}"),
        );
    }
    ui.click("one");
    ui.click("▸12");
    let visible = sidebar(&ui)
        .positions()
        .any(|p| ui.hit_at(p.x, p.y) == Some(Target::Session(sid("s1"))));
    assert!(visible, "one is out of view:\n{}", ui.screen());
    insta::assert_snapshot!(ui.region(sidebar(&ui)));
    // Wheeled back up, the session with the tree can be selected, and its tree runs off the bottom.
    ui.scroll("one", -20);
    ui.click("two");
    assert_eq!(ui.client().selected(), Some(&sid("s2")));
    ui.assert_contains("├ ● builder  task 0")
        .assert_lacks("task 11")
        .assert_lacks("one");
}

#[test]
fn subagents_starting_above_the_selection_keep_it_visible() {
    let mut ui = pm_with_a_team(80, 16);
    ui.click("▸4");
    ui.click("docs");
    for i in 5..=11 {
        started(
            &mut ui,
            "s2",
            &format!("a{i}"),
            "builder",
            &format!("task {i}"),
        );
        let visible = sidebar(&ui)
            .positions()
            .any(|p| ui.hit_at(p.x, p.y) == Some(Target::Session(sid("s1"))));
        assert!(visible, "docs is out of view after a{i}:\n{}", ui.screen());
    }
}

#[test]
fn a_tiny_window_with_a_tree() {
    let mut ui = pm_with_a_team(80, 14);
    blocked(&mut ui, "s2", "a3");
    ui.click("▸4");
    // Collapsed: each subagent's glyph under its session's, with the branch in the attention column.
    ui.resize(40, 12);
    insta::assert_snapshot!(ui.region(sidebar(&ui)));
    for (cols, rows) in [(20, 4), (3, 3), (1, 1)] {
        ui.resize(cols, rows);
    }
}

#[test]
fn agent_text_in_the_tree_is_drawn_safe_and_clipped() {
    let mut ui = working(80, 10);
    started(
        &mut ui,
        "s2",
        "a1",
        "a-very-long-agent-type\u{202E}\x1b[31m",
        "a task far longer than the sidebar\u{7}\u{200D}",
    );
    ui.click("▸1");
    insta::assert_snapshot!(ui.region(sidebar(&ui)));
    let text = ui.screen();
    assert!(!text.contains('\u{202E}') && !text.contains('\x1b') && !text.contains('\u{7}'));
}
