//! The status page's preview (T58): the selected session's live screen beside the table, read-only, and the question
//! its agent asks with a button for each answer the host would accept, answered by a click or a key.

use std::num::NonZeroU32;

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use shepherd_core::acpp::{Choice, PromptBy, Source, SourceKind};
use shepherd_core::client::{ClientCommand, ClientEvent, PeerEvent};
use shepherd_core::ids::MachineId;
use shepherd_core::wire::{
    ClientMessage, ClientNotification, ClientRequest, Method, SessionAnswer, Status, WireError,
};
use shepherd_testkit::builders::{ClientDriver, HostDriver, claude_session_spec};
use shepherd_tui::Target;

use crate::harness::Ui;
use crate::status::{hook, open_status, pane, reach, sid, status_page, table};

/// The team's page in a window wide enough for the preview.
fn wide() -> Ui {
    status_page(160, 40)
}

/// The inside of the preview's box, titled with the selected session's name.
fn preview(ui: &Ui, name: &str) -> Rect {
    ui.boxed(name)
}

fn asked(ui: &mut Ui, session: &str, params: Value) {
    hook(ui, session, "_shepherd/agent/asked", params);
}

/// `t28-hello-build` (selected, asking permission for `Bash(cargo test)`) says what it asks, for a `tool` prompt.
fn t28_asks(ui: &mut Ui, tool: &str) {
    asked(
        ui,
        "s4",
        json!({ "kind": "permission", "text": "Bash: cargo test", "tool": tool }),
    );
}

fn answers(ui: &Ui) -> Vec<SessionAnswer> {
    ui.requests()
        .into_iter()
        .filter_map(|r| match r {
            ClientRequest::SessionAnswer(a) => Some(a.clone()),
            _ => None,
        })
        .collect()
}

/// The seq of the question `session` is asked now.
fn seq(ui: &Ui, session: &str) -> shepherd_core::acpp::Seq {
    ui.client()
        .state(&sid(session))
        .and_then(|s| s.question_seq())
        .unwrap_or_else(|| panic!("a pending question"))
}

/// What was typed into any session.
fn inputs(ui: &Ui) -> usize {
    ui.sent()
        .iter()
        .filter(|c| {
            matches!(
                c,
                ClientCommand::Send(ClientMessage::Notification(
                    ClientNotification::SessionInput(_)
                ))
            )
        })
        .count()
}

#[test]
fn the_preview_shows_the_selected_sessions_live_screen_and_follows_the_selection() {
    let mut ui = wide();
    ui.pty("s4", b"running the wire tests\r\n");
    ui.pty("s1", b"all done, at the prompt\r\n");
    let shown = ui.region(preview(&ui, "t28-hello-build"));
    assert!(shown.contains("running the wire tests"), "{shown}");
    assert!(!shown.contains("all done"));
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    let shown = ui.region(preview(&ui, "fix-login"));
    assert!(shown.contains("all done, at the prompt"), "{shown}");
    // Live: more output shows.
    ui.pty("s1", b"and more\r\n");
    assert!(ui.region(preview(&ui, "fix-login")).contains("and more"));
}

#[test]
fn the_preview_shows_the_bottom_of_a_screen_taller_than_it() {
    let mut ui = wide();
    let lines: String = (1..=60).map(|i| format!("line {i}\r\n")).collect();
    ui.pty("s4", lines.as_bytes());
    ui.pty("s4", b"the prompt is here");
    let shown = ui.region(preview(&ui, "t28-hello-build"));
    assert!(shown.contains("the prompt is here"), "{shown}");
}

#[test]
fn the_preview_with_a_question_and_its_buttons() {
    let mut ui = wide();
    ui.pty(
        "s4",
        b"Bash command\r\n\r\n  cargo test\r\n\r\nDo you want to proceed?\r\n",
    );
    t28_asks(&mut ui, "Edit");
    insta::assert_snapshot!(ui.region(pane(&ui)));
}

#[test]
fn answer_buttons_appear_only_while_a_question_is_pending() {
    let mut ui = wide();
    // Waiting on the user, but its agent hasn't said what it asks: no buttons, attach to answer.
    let area = preview(&ui, "t28-hello-build");
    assert!(ui.region(area).contains("Bash(cargo test)"));
    assert!(ui.find_in(area, "[y yes]").is_empty());
    assert!(ui.region(area).contains("attach to answer it in its pane"));
    t28_asks(&mut ui, "Edit");
    let area = preview(&ui, "t28-hello-build");
    for button in ["[y yes]", "[a always]", "[n no]", "[⏎ attach]"] {
        assert!(!ui.find_in(area, button).is_empty(), "{button} missing");
    }
    // Another session, asking nothing: no answer buttons.
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    let area = preview(&ui, "fix-login");
    assert!(ui.find_in(area, "[y yes]").is_empty());
    assert!(!ui.find_in(area, "[⏎ attach]").is_empty());
    // Back to it; then the user answers in its pane and the agent goes on.
    let at = ui.at_in(table(&ui), "t28-hello-build");
    ui.click_at(at.x, at.y);
    hook(
        &mut ui,
        "s4",
        "session/update",
        json!({ "sessionId": "a1", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t9",
            "status": "in_progress" } }),
    );
    assert!(
        ui.find_in(preview(&ui, "t28-hello-build"), "[y yes]")
            .is_empty()
    );
}

#[test]
fn always_is_shown_disabled_where_the_host_refuses_it_and_says_why() {
    let mut ui = wide();
    t28_asks(&mut ui, "Bash");
    let area = preview(&ui, "t28-hello-build");
    assert!(ui.find_in(area, "[a always]").is_empty());
    let at = ui.at_in(area, "a always");
    assert!(
        !matches!(ui.hit_at(at.x, at.y), Some(Target::StatusAnswer { .. })),
        "not clickable"
    );
    assert!(
        ui.region(area).contains("doesn't offer always"),
        "{}",
        ui.region(area)
    );
    // Its key sends nothing, and says why.
    ui.key(KeyCode::Char('a'));
    assert!(answers(&ui).is_empty());
    assert!(
        ui.region(preview(&ui, "t28-hello-build"))
            .contains("always refused")
    );
}

#[test]
fn clicking_a_button_answers_the_question_shown_by_client() {
    let mut ui = wide();
    t28_asks(&mut ui, "Edit");
    let asked = seq(&ui, "s4");
    let area = preview(&ui, "t28-hello-build");
    let at = ui.at_in(area, "[y yes]");
    assert!(matches!(
        ui.hit_at(at.x, at.y),
        Some(Target::StatusAnswer {
            choice: Choice::Yes,
            ..
        })
    ));
    ui.click_at(at.x, at.y);
    assert_eq!(
        answers(&ui),
        [SessionAnswer {
            session: sid("s4"),
            choice: Choice::Yes,
            asked: Some(asked),
            by: Some(PromptBy::Client),
        }]
    );
    ui.assert_contains("answering yes…");
    ui.answer(Method::SessionAnswer, Ok(json!({})));
    ui.assert_contains("answered yes");
}

#[test]
fn a_key_answers_the_question_shown() {
    let mut ui = wide();
    t28_asks(&mut ui, "Edit");
    ui.key(KeyCode::Char('n'));
    let sent = answers(&ui);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].choice, Choice::No);
    assert_eq!(sent[0].asked, Some(seq(&ui, "s4")));
}

#[test]
fn a_stale_answer_is_refused_and_says_so() {
    let mut ui = wide();
    t28_asks(&mut ui, "Edit");
    ui.click_in(preview(&ui, "t28-hello-build"), "[a always]");
    ui.answer(
        Method::SessionAnswer,
        Err(WireError::NothingAsked {
            session: sid("s4"),
            status: Status::Running,
        }),
    );
    let shown = ui.region(preview(&ui, "t28-hello-build"));
    assert!(
        shown.contains("always refused: it was already answered"),
        "{shown}"
    );
}

#[test]
fn a_question_shows_its_options_as_buttons() {
    let mut ui = wide();
    hook(
        &mut ui,
        "s1",
        "session/update",
        json!({ "sessionId": "a1", "update": { "sessionUpdate": "tool_call", "toolCallId": "q1",
            "title": "AskUserQuestion", "status": "in_progress" } }),
    );
    ui.on_host(|h| {
        h.user_prompt("s1", "pick an ALPN");
    });
    asked(
        &mut ui,
        "s1",
        json!({ "kind": "question", "text": "Which ALPN?", "choices": ["h3", "shepherd/1"],
                "tool": "AskUserQuestion" }),
    );
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    let area = preview(&ui, "fix-login");
    assert!(
        ui.region(area).contains("Which ALPN?"),
        "{}",
        ui.region(area)
    );
    for button in ["[1 h3]", "[2 shepherd/1]", "[n no]"] {
        assert!(!ui.find_in(area, button).is_empty(), "{button} missing");
    }
    // `y` isn't an answer a question takes: nothing is sent, and nothing is said.
    ui.key(KeyCode::Char('y'));
    assert!(answers(&ui).is_empty());
    assert!(ui.client().answer_report().is_none());
    ui.key(KeyCode::Char('2'));
    let two = Choice::Option(NonZeroU32::new(2).unwrap_or_else(|| panic!("2")));
    assert_eq!(answers(&ui).last().map(|a| a.choice), Some(two));
}

#[test]
fn the_question_text_is_drawn_safe_and_clipped() {
    let mut ui = wide();
    ui.on_host(|h| {
        h.user_prompt("s1", "pick");
    });
    let evil = format!("ok\u{1b}[2J\u{202e}gone\u{7}{}", "x".repeat(300));
    asked(
        &mut ui,
        "s1",
        json!({ "kind": "question", "text": evil, "choices": ["a"], "tool": "AskUserQuestion" }),
    );
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    let shown = ui.region(preview(&ui, "fix-login"));
    assert!(!shown.contains('\u{1b}') && !shown.contains('\u{202e}') && !shown.contains('\u{7}'));
    let line = shown
        .lines()
        .find(|l| l.contains("ok[2Jgone"))
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(line.trim_end().ends_with('…'), "cut at the box: {line}");
    // Nothing spills past the box.
    preview(&ui, "fix-login");
}

#[test]
fn an_option_text_is_drawn_safe_and_clipped() {
    let mut ui = wide();
    ui.on_host(|h| {
        h.user_prompt("s1", "pick");
    });
    let evil = format!("\u{1b}]52;c;bad\u{7}{}", "y".repeat(200));
    asked(
        &mut ui,
        "s1",
        json!({ "kind": "question", "text": "Which?", "choices": [evil], "tool": "AskUserQuestion" }),
    );
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    let area = preview(&ui, "fix-login");
    let shown = ui.region(area);
    assert!(
        !shown.contains('\u{1b}') && !shown.contains('\u{7}'),
        "{shown}"
    );
    assert!(shown.contains("[1 ]52;c;bad"), "{shown}");
    assert!(shown.contains('…'), "cut: {shown}");
}

#[test]
fn the_attach_button_and_enter_attach_the_selected_session() {
    let mut ui = wide();
    ui.click_in(preview(&ui, "t28-hello-build"), "[⏎ attach]");
    assert_eq!(ui.client().selected(), Some(&sid("s4")));
    let mut ui = wide();
    ui.key(KeyCode::Enter);
    assert_eq!(ui.client().selected(), Some(&sid("s4")));
}

#[test]
fn the_preview_is_read_only() {
    let mut ui = wide();
    // The session asks for the mouse: still, nothing reaches it from the preview.
    ui.pty("s4", b"\x1b[?1000h\x1b[?1006hhello\r\n");
    t28_asks(&mut ui, "Edit");
    let area = preview(&ui, "t28-hello-build");
    let at = ui.at_in(area, "hello");
    assert!(matches!(ui.hit_at(at.x, at.y), Some(Target::Pane(_))));
    ui.click_at(at.x, at.y);
    ui.scroll_at(at.x, at.y, 2);
    ui.type_text("xqz");
    ui.key(KeyCode::Esc);
    ui.paste("pasted");
    assert_eq!(inputs(&ui), 0, "nothing typed into a session");
    assert!(answers(&ui).is_empty());
}

#[test]
fn a_120_column_window_shows_the_preview_beside_a_narrowed_table() {
    for rows in [30, 36] {
        let ui = status_page(120, rows);
        ui.assert_contains("[⏎ attach]");
        // The table gives up columns, the subagents and the agent first, and keeps whole names.
        let area = table(&ui);
        for name in ["f36-cleanup-view", "t28-hello-build", "fix-login"] {
            assert!(!ui.find_in(area, name).is_empty(), "{name} at 120x{rows}");
        }
        for kept in ["MACHINE", "DOING", "PR"] {
            assert!(!ui.find_in(area, kept).is_empty(), "{kept} at 120x{rows}");
        }
        assert!(ui.find_in(area, "SUB").is_empty() && ui.find_in(area, "AGENT").is_empty());
    }
}

#[test]
fn the_page_at_120x36_matches_the_mock() {
    let mut ui = status_page(120, 36);
    ui.pty("s4", b"Bash command\r\n\r\n  cargo test -p shepherd-core wire\r\n\r\nDo you want to proceed?\r\n");
    t28_asks(&mut ui, "Bash");
    insta::assert_snapshot!(ui.region(pane(&ui)));
}

#[test]
fn a_narrow_pane_drops_the_preview_before_the_table() {
    // Even the narrowed table wouldn't fit beside the preview.
    let mut ui = status_page(80, 24);
    ui.assert_lacks("[⏎ attach]");
    assert!(
        !ui.find_in(table(&ui), "AGENT").is_empty(),
        "the table keeps its columns"
    );
    // A window too short for the preview's lines keeps the table whole.
    ui.resize(160, 9);
    assert!(!ui.find_in(table(&ui), "t28-hello-build").is_empty());
    ui.assert_lacks("[⏎ attach]");
    ui.resize(160, 40);
    ui.assert_contains("[⏎ attach]");
}

#[test]
fn an_answers_note_shows_only_for_its_session() {
    let mut ui = wide();
    t28_asks(&mut ui, "Edit");
    ui.click_in(preview(&ui, "t28-hello-build"), "[y yes]");
    assert!(
        ui.region(preview(&ui, "t28-hello-build"))
            .contains("answering yes…")
    );
    let at = ui.at_in(table(&ui), "fix-login");
    ui.click_at(at.x, at.y);
    ui.assert_lacks("answering yes…");
}

#[test]
fn a_page_with_no_sessions_keeps_the_table_full_width() {
    let mut driver = ClientDriver::new();
    driver.connect_named("studio");
    let mut ui = Ui::new(driver, 160, 40);
    open_status(&mut ui);
    // The table's box spans the pane: its inside is the pane less a border column each side.
    assert_eq!(table(&ui).width, pane(&ui).width - 2, "{}", ui.screen());
    ui.assert_lacks("[⏎ attach]");
}

#[test]
fn a_question_on_a_machine_that_cannot_be_reached_has_no_buttons() {
    let mut c = ClientDriver::new();
    let subscribe = c.connect_named("studio");
    c.sync();
    let caught_up = c.answer(&subscribe, Ok(json!({})));
    c.answer(
        &caught_up,
        Ok(json!({ "peers": [{ "id": "m_tower", "name": "tower" }] })),
    );
    let mut host = HostDriver::new();
    host.stamp_machine("m_tower");
    host.new_session_with("far", claude_session_spec());
    host.rename("far", "far");
    host.user_prompt("far", "work");
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    host.append(
        Some("far"),
        source.clone(),
        json!({ "jsonrpc": "2.0", "id": 9, "method": "session/request_permission",
            "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t9", "title": "Edit(x)" }, "options": [] } }),
    );
    host.append(
        Some("far"),
        source,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/asked",
            "params": { "kind": "permission", "text": "Edit: x", "tool": "Edit" } }),
    );
    reach(&mut c, "m_tower", &host);
    let mut ui = Ui::new(c, 160, 40);
    open_status(&mut ui);
    // Reachable: it can be answered.
    assert!(
        !ui.find_in(preview(&ui, "far"), "[y yes]").is_empty(),
        "{}",
        ui.screen()
    );
    ui.client_event(ClientEvent::Peer {
        machine: MachineId::new("m_tower"),
        event: PeerEvent::Disconnected,
    });
    let area = preview(&ui, "far");
    assert!(
        ui.find_in(area, "[y yes]").is_empty(),
        "{}",
        ui.region(area)
    );
    assert!(
        ui.region(area).contains("tower can't be reached now"),
        "{}",
        ui.region(area)
    );
    ui.key(KeyCode::Char('y'));
    assert!(answers(&ui).is_empty());
}
