//! The status page's events feed and message line (T59): the latest events across sessions, newest last, each
//! clickable to select its session; a message line that prompts the selected agent or a group, and reports who refused.

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use serde_json::json;
use shepherd_core::acpp::PromptBy;
use shepherd_core::client::message::MessageTarget;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{Busy, ClientRequest, Method, SessionPrompt, WireError};
use shepherd_tui::Target;

use crate::harness::Ui;
use crate::status::{hook, pane, shepherd_frame, sid, status_page, table};

/// The inside of the events feed's box.
fn feed(ui: &Ui) -> Rect {
    ui.boxed("events")
}

/// The message line: the pane's last line.
fn message_line(ui: &Ui) -> Rect {
    let area = pane(ui);
    Rect::new(area.x, area.bottom() - 1, area.width, 1)
}

/// The prompts the client has asked hosts for, in order.
fn prompts(ui: &Ui) -> Vec<SessionPrompt> {
    ui.requests()
        .into_iter()
        .filter_map(|r| match r {
            ClientRequest::SessionPrompt(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// The team's page, with `fix-login` told something by the project manager's agent (which starts its turn), and a tick
/// a minute after the team was made: `t28-hello-build` waits on the user, `docs-arch`, `f36-cleanup-view` and
/// `fix-login` work, `wire-v2` exited.
fn busy_team() -> Ui {
    busy_team_in(120, 30)
}

/// [`busy_team`] in a `cols` x `rows` window.
fn busy_team_in(cols: u16, rows: u16) -> Ui {
    let mut ui = status_page(cols, rows);
    ui.on_host(|h| {
        shepherd_frame(
            h,
            "s1",
            "_shepherd/session/prompted",
            json!({ "text": "rebase on main", "by": "mcp:s9" }),
        );
    });
    ui.tick("2026-10-29T12:01:00.000Z");
    ui
}

fn busy(status: Busy) -> WireError {
    WireError::SessionBusy {
        session: SessionId::new("x"),
        status,
    }
}

// ---------- the feed ----------

#[test]
fn the_feed_shows_the_latest_events_newest_last_with_their_age() {
    let ui = busy_team();
    let lines: Vec<String> = ui.region(feed(&ui)).lines().map(str::to_owned).collect();
    let last = lines.last().cloned().unwrap_or_default();
    assert!(
        last.contains("fix-login")
            && last.contains("prompted")
            && last.contains("by mcp:s9: rebase on main"),
        "the newest is last: {last}"
    );
    assert!(
        last.trim_start().starts_with("59s") || last.trim_start().starts_with("1m"),
        "{last}"
    );
    insta::assert_snapshot!(ui.region(feed(&ui)));
}

#[test]
fn clicking_a_feed_line_selects_its_session_in_the_table() {
    let mut ui = busy_team();
    assert_eq!(ui.client().status_selected(), Some(&sid("s4")));
    let at = ui.at_in(feed(&ui), "wire-v2");
    assert!(matches!(
        ui.hit_at(at.x, at.y),
        Some(Target::StatusEvent { session, .. }) if session == sid("s2")
    ));
    let sidebar = ui.client().selected().cloned();
    ui.click_at(at.x, at.y);
    assert_eq!(ui.client().status_selected(), Some(&sid("s2")));
    assert_eq!(
        ui.client().selected(),
        sidebar.as_ref(),
        "the sidebar's stays"
    );
    // Anywhere on the line.
    let other = ui.at_in(feed(&ui), "rebase on main");
    ui.click_at(other.x, other.y);
    assert_eq!(ui.client().status_selected(), Some(&sid("s1")));
}

#[test]
fn a_closed_sessions_line_stays_and_selects_nothing() {
    let mut ui = busy_team();
    ui.on_host(|h| {
        h.close("s2");
        h.pty_exited("s2", Some(1));
    });
    let at = ui.at_in(feed(&ui), "closed");
    ui.click_at(at.x, at.y);
    assert_eq!(ui.client().status_selected(), Some(&sid("s4")));
    ui.assert_lacks("panicked");
}

#[test]
fn agent_text_in_the_feed_is_drawn_safe_and_clipped() {
    let mut ui = busy_team();
    let evil = format!("ok\u{1b}[2J\u{202e}gone\u{7}{}", "x".repeat(300));
    ui.on_host(|h| {
        shepherd_frame(
            h,
            "s3",
            "_shepherd/session/prompted",
            json!({ "text": evil, "by": "mcp:\u{1b}]52;c;bad\u{7}" }),
        );
    });
    let shown = ui.region(feed(&ui));
    assert!(!shown.contains('\u{1b}') && !shown.contains('\u{202e}') && !shown.contains('\u{7}'));
    assert!(shown.contains("ok[2Jgone"), "{shown}");
    let line = shown.lines().last().unwrap_or_default().to_owned();
    assert!(line.ends_with('…'), "cut at the box: {line}");
    // Nothing spills past the box.
    ui.boxed("events");
}

#[test]
fn an_ask_and_its_answer_are_in_the_feed() {
    let mut ui = busy_team();
    hook(
        &mut ui,
        "s4",
        "session/update",
        json!({ "sessionId": "a1", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t9",
            "status": "in_progress" } }),
    );
    let shown = ui.region(feed(&ui));
    let last = shown.lines().last().unwrap_or_default();
    assert!(
        last.contains("t28-hello-build") && last.contains("answered"),
        "{shown}"
    );
}

// ---------- the message line ----------

#[test]
fn clicking_the_message_line_and_typing_prompts_the_selected_agent_by_client() {
    let mut ui = busy_team();
    ui.click_in(message_line(&ui), "click to message");
    assert!(ui.client().composing());
    ui.type_text("just rerun the tests");
    // Keys go to the line, not the table: `j` was typed, not a move.
    assert_eq!(ui.client().status_selected(), Some(&sid("s4")));
    assert!(
        ui.region(message_line(&ui))
            .contains("just rerun the tests")
    );
    ui.key(KeyCode::Enter);
    assert_eq!(
        prompts(&ui),
        [SessionPrompt {
            session: sid("s4"),
            text: "just rerun the tests".to_owned(),
            by: Some(PromptBy::Client),
        }]
    );
    ui.assert_contains("sending to 1…");
    ui.answer(Method::SessionPrompt, Err(busy(Busy::WantsUser)));
    let line = ui.region(message_line(&ui));
    assert!(
        line.contains("refused: t28-hello-build (wants-user)"),
        "{line}"
    );
}

#[test]
fn esc_gives_the_keys_back_to_the_table() {
    let mut ui = busy_team();
    ui.click_in(message_line(&ui), "click to message");
    ui.key(KeyCode::Esc);
    assert!(!ui.client().composing());
    ui.key(KeyCode::Down);
    assert_ne!(ui.client().status_selected(), Some(&sid("s4")));
}

#[test]
fn a_paste_goes_into_the_message_line_while_it_takes_the_keys() {
    let mut ui = busy_team();
    ui.paste("not yet");
    assert_eq!(ui.client().message_text(), "");
    ui.click_in(message_line(&ui), "click to message");
    ui.paste("first line\r\nsecond\rthird");
    assert_eq!(ui.client().message_text(), "first line\nsecond\nthird");
    assert!(
        ui.region(message_line(&ui))
            .contains("first line↵second↵third"),
        "{}",
        ui.region(message_line(&ui))
    );
    ui.key(KeyCode::Enter);
    let sent: Vec<String> = prompts(&ui).into_iter().map(|p| p.text).collect();
    assert_eq!(
        sent,
        ["first line\nsecond\nthird"],
        "the whole paste is sent"
    );
}

#[test]
fn tab_and_the_target_widen_the_target() {
    let mut ui = busy_team();
    ui.assert_contains("[to t28-hello-build]");
    ui.key(KeyCode::Tab);
    assert_eq!(ui.client().message_target(), &MessageTarget::Working);
    ui.assert_contains("[to all working (3)]");
    let at = ui.at_in(message_line(&ui), "[to all working (3)]");
    assert!(matches!(
        ui.hit_at(at.x, at.y),
        Some(Target::StatusTarget {
            next: MessageTarget::Idle,
            ..
        })
    ));
    ui.click_at(at.x, at.y);
    ui.assert_contains("[to all idle (0)]");
    ui.click("[to all idle (0)]");
    assert_eq!(
        ui.client().message_target(),
        &MessageTarget::Machine(MachineId::new("m_test"))
    );
    ui.assert_contains("[to all on studio (5)]");
    ui.click("[to all on studio (5)]");
    assert_eq!(ui.client().message_target(), &MessageTarget::Selected);
}

#[test]
fn a_group_send_reports_who_was_refused_and_why() {
    // Wide enough for the whole report.
    let mut ui = busy_team_in(180, 30);
    ui.click("[to t28-hello-build]")
        .click("[to all working (3)]")
        .click("[to all idle (0)]");
    ui.click_in(message_line(&ui), "click to message");
    ui.type_text("status?");
    ui.click("[send]");
    let to: Vec<SessionId> = prompts(&ui).into_iter().map(|p| p.session).collect();
    // Every claude session on studio but the exited one; docs-arch runs no agent.
    assert_eq!(to.len(), 3, "{to:?}");
    assert!(
        !to.contains(&sid("s2")) && !to.contains(&sid("s5")),
        "{to:?}"
    );
    // Answered newest first: one refused busy, one waiting on the user, one sent.
    ui.answer(Method::SessionPrompt, Err(busy(Busy::Running)));
    ui.answer(Method::SessionPrompt, Err(busy(Busy::WantsUser)));
    ui.answer(Method::SessionPrompt, Ok(json!({})));
    insta::assert_snapshot!(ui.region(message_line(&ui)));
    let line = ui.region(message_line(&ui));
    assert!(line.contains("sent to 1"), "{line}");
    assert!(
        line.contains("(busy)") && line.contains("(wants-user)"),
        "{line}"
    );
    assert!(
        line.contains("skipped: docs-arch (no agent), wire-v2 (exited)"),
        "{line}"
    );
}

#[test]
fn the_send_button_with_nothing_typed_sends_nothing() {
    let mut ui = busy_team();
    ui.click("[send]");
    assert!(prompts(&ui).is_empty());
    ui.assert_contains("click to message");
}

#[test]
fn the_page_keeps_its_table_in_a_small_window() {
    let mut ui = busy_team();
    for (cols, rows) in [(40, 9), (24, 9), (80, 12), (120, 14)] {
        ui.resize(cols, rows);
        let _ = ui.screen();
    }
    ui.resize(80, 16);
    // The table first: the feed only once the table keeps its lines, the message line from nine rows.
    assert!(!ui.find_in(table(&ui), "t28-hel").is_empty());
    insta::assert_snapshot!(ui.screen());
}
