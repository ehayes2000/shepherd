//! The harness's own steps and assertions: each does what a user's action does, and a click on something that isn't
//! drawn once fails, saying what is.

use std::panic::{AssertUnwindSafe, catch_unwind};

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use serde_json::json;
use shepherd_core::client::Popup;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{ClientRequest, Method};

use crate::harness::Ui;

/// The message `f` panics with. Panics if it doesn't.
fn panic_message(f: impl FnOnce()) -> String {
    let Err(panic) = catch_unwind(AssertUnwindSafe(f)) else {
        panic!("expected a panic");
    };
    match (panic.downcast_ref::<String>(), panic.downcast_ref::<&str>()) {
        (Some(s), _) => s.clone(),
        (_, Some(s)) => (*s).to_owned(),
        _ => panic!("a panic without a message"),
    }
}

fn selected(ui: &Ui) -> Option<&str> {
    ui.client().selected().map(SessionId::as_str)
}

// ---------- clicks ----------

#[test]
fn click_presses_the_drawn_label() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 80, 24);
    assert_eq!(selected(&ui), Some("s2"));
    ui.click("fix-login");
    assert_eq!(selected(&ui), Some("s1"));
}

#[test]
fn clicking_a_label_that_is_not_drawn_fails_showing_the_screen() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    let message = panic_message(|| {
        ui.click("no-such-label");
    });
    assert!(
        message.contains("\"no-such-label\" is not on screen"),
        "{message}"
    );
    assert!(message.contains("+ New session"), "{message}");
    assert!(message.contains("fix-login"), "{message}");
}

#[test]
fn clicking_a_label_drawn_twice_fails_naming_both_places() {
    let mut ui = Ui::with_sessions(&["twin", "twin"], 80, 24);
    let message = panic_message(|| {
        ui.click("twin");
    });
    assert!(message.contains("\"twin\" is ambiguous"), "{message}");
    let places = ui.find("twin");
    assert_eq!(places.len(), 2);
    for p in places {
        assert!(
            message.contains(&format!("({}, {})", p.x, p.y)),
            "{message}"
        );
    }
}

#[test]
fn click_in_picks_the_label_inside_an_area() {
    let mut ui = Ui::with_sessions(&["twin", "twin"], 80, 24);
    let lower = ui.find("twin")[1];
    ui.click_in(Rect::new(0, lower.y, 24, 2), "twin");
    assert_eq!(selected(&ui), Some("s1"));
}

#[test]
fn click_at_presses_the_cell() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 80, 24);
    let row = ui.at("fix-login");
    // The second line of the row is the row too.
    ui.click_at(row.x, row.y + 1);
    assert_eq!(selected(&ui), Some("s1"));
}

#[test]
fn right_click_opens_the_rows_menu() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.assert_lacks("Rename");
    ui.right_click("fix-login");
    ui.assert_contains("Rename");
}

#[test]
fn scroll_turns_the_wheel_over_the_label() {
    let names: Vec<String> = (1..=15).map(|i| format!("name-{i:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut ui = Ui::with_sessions(&names, 80, 12);
    ui.assert_lacks("name-01");
    ui.scroll("name-15", 20);
    ui.assert_contains("name-01").assert_lacks("name-15");
    ui.scroll("name-01", -20);
    ui.assert_contains("name-15");
}

// ---------- keys ----------

#[test]
fn type_text_and_key_go_to_the_open_popup() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 80, 24);
    ui.click("⌕ Search").type_text("doc");
    assert_eq!(
        ui.client().popup(),
        Some(&Popup::Finder {
            query: "doc".into()
        })
    );
    ui.key(KeyCode::Esc);
    assert_eq!(ui.client().popup(), None);
}

#[test]
fn paste_types_into_the_open_popup() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.click("⌕ Search").paste("fix\nrest");
    assert_eq!(
        ui.client().popup(),
        Some(&Popup::Finder {
            query: "fix".into()
        })
    );
}

#[test]
fn ctrl_sends_the_prefix() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.ctrl('b').type_text("?");
    assert_eq!(ui.client().popup(), Some(&Popup::Help));
}

// ---------- the window ----------

#[test]
fn resize_redraws_at_the_new_size() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.resize(40, 12);
    assert_eq!(ui.window(), Rect::new(0, 0, 40, 12));
    ui.assert_lacks("+ New session").assert_contains("+");
    ui.resize(1, 1);
    assert_eq!(ui.window(), Rect::new(0, 0, 1, 1));
    assert!(ui.screen().chars().count() <= 1, "{:?}", ui.screen());
}

#[test]
fn screen_and_region_read_the_drawn_text() {
    let ui = Ui::with_sessions(&["fix-login"], 80, 24);
    let row = ui.at("fix-login");
    assert_eq!(ui.region(Rect::new(row.x, row.y, 9, 1)), "fix-login");
    assert_eq!(ui.screen().lines().count(), 24);
    assert!(ui.screen().lines().all(|l| l.chars().count() <= 80));
    assert!(ui.contains("fix-login"));
    assert!(!ui.contains("fix-logout"));
}

#[test]
fn wide_characters_are_found_as_they_read() {
    let ui = Ui::with_sessions(&["日本語-docs"], 80, 24);
    let at = ui.at("日本語-docs");
    assert_eq!(ui.region(Rect::new(at.x, at.y, 11, 1)), "日本語-docs");
    assert_eq!(ui.at("-docs").x, at.x + 6);
}

// ---------- the host ----------

#[test]
fn on_host_delivers_what_the_host_appended() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.on_host(|h| {
        h.new_session("s2");
        h.rename("s2", "docs");
    });
    ui.assert_contains("docs");
}

#[test]
fn host_pty_output_is_drawn_in_the_pane() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.pty("s1", b"hello from the shell");
    ui.assert_contains("hello from the shell");
}

#[test]
fn answer_replies_to_the_request_the_ui_sent() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.click("⌕ Search");
    assert!(
        ui.requests()
            .iter()
            .any(|r| matches!(r, ClientRequest::WorktreeList(_))),
        "{:?}",
        ui.requests()
    );
    ui.answer(
        Method::WorktreeList,
        Ok(json!({ "worktrees": [{ "path": "/wt/shepherd/old-idea", "branch": "old-idea" }] })),
    );
    ui.assert_contains("old-idea");
}

#[test]
fn answering_a_request_never_sent_fails_naming_what_was() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 24);
    ui.click("⌕ Search");
    let message = panic_message(|| {
        ui.answer(Method::SessionClose, Ok(json!({})));
    });
    assert!(message.contains("no unanswered SessionClose"), "{message}");
    assert!(message.contains("WorktreeList"), "{message}");
}

// ---------- snapshots ----------

#[test]
fn the_screen_snapshots_at_any_step() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 60, 10);
    insta::assert_snapshot!("two_sessions", ui.screen());
    ui.click("fix-login").right_click("fix-login");
    insta::assert_snapshot!("menu_over_fix_login", ui.screen());
}
