//! `render`: the sidebar, the selected session's terminal, popups and help, drawn from client state.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use serde_json::{Value, json};
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::{Client, Focus, Intent};
use shepherd_core::ids::SessionId;
use shepherd_term::Screen;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Screens, layout, pane_size, render};

fn draw(client: &Client, screens: &Screens, cols: u16, rows: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(client, screens, frame))
        .unwrap_or_else(|e| match e {});
    terminal
}

fn buffer(client: &Client, screens: &Screens, cols: u16, rows: u16) -> Buffer {
    draw(client, screens, cols, rows).backend().buffer().clone()
}

fn hook() -> Source {
    Source::new(SourceKind::Hook, Some("claude-code".to_owned()))
}

fn frame(c: &mut ClientDriver, session: &str, frame: Value) {
    c.host().append(Some(session), hook(), frame);
}

fn prompt(c: &mut ClientDriver, session: &str) {
    frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": "go" }] } }),
    );
}

fn idle(c: &mut ClientDriver, session: &str) {
    frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
}

fn permission(c: &mut ClientDriver, session: &str) {
    frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": "rm -rf build" },
                            "options": [] } }),
    );
}

/// Three named sessions: `fix-login` working, `docs` idle and unseen, `refactor` (newest, selected) waiting for
/// permission. The selected one has printed a little.
fn three_sessions() -> (ClientDriver, Screens) {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    for (id, name) in [("s1", "fix-login"), ("s2", "docs"), ("s3", "refactor")] {
        c.host().rename(id, name);
    }
    prompt(&mut c, "s1");
    prompt(&mut c, "s2");
    idle(&mut c, "s2");
    prompt(&mut c, "s3");
    permission(&mut c, "s3");
    c.sync();
    let mut screen = Screen::new(56, 24);
    screen.feed(
        b"$ cargo build\r\n\x1b[32m   Compiling\x1b[0m shepherd\r\nAllow rm -rf build? (y/n) ",
    );
    let screens = Screens::from([(SessionId::new("s3"), screen)]);
    (c, screens)
}

// ---------- snapshots ----------

#[test]
fn empty_state_hints_how_to_create_a_session() {
    let mut c = ClientDriver::new();
    c.connect();
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 24));
}

#[test]
fn three_sessions_with_mixed_status() {
    let (c, screens) = three_sessions();
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

#[test]
fn rename_input() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::BeginRename);
    c.user(Intent::EditRename("refactor-api".into()));
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

#[test]
fn close_confirmation() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::BeginClose);
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

#[test]
fn help_popup() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::ShowHelp);
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

#[test]
fn narrow_terminal_collapses_the_sidebar_to_glyphs() {
    let (c, screens) = three_sessions();
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 40, 12));
}

#[test]
fn prefix_pending() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::SetFocus(Focus::Command));
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

// ---------- layout and the pane ----------

/// A screen of `size` with every cell an `X`.
fn full_screen(cols: u16, rows: u16) -> Screen {
    let mut screen = Screen::new(cols, rows);
    for row in 1..=rows {
        screen.feed(format!("\x1b[{row};1H").as_bytes());
        screen.feed("X".repeat(usize::from(cols)).as_bytes());
    }
    screen
}

#[test]
fn the_pane_size_sent_to_the_session_is_exactly_the_area_it_is_drawn_in() {
    let c = ClientDriver::with_sessions(&["s1"]);
    let sizes = [
        (120, 40),
        (80, 24),
        (60, 20),
        (59, 20),
        (40, 12),
        (6, 3),
        (5, 1),
        (4, 1),
        (2, 1),
    ];
    for (cols, rows) in sizes {
        let size = pane_size(cols, rows);
        let pane = layout(Rect::new(0, 0, cols, rows)).pane;
        assert_eq!((size.cols, size.rows), (pane.width, pane.height));
        let screens = Screens::from([(SessionId::new("s1"), full_screen(size.cols, size.rows))]);
        let buf = buffer(c.client(), &screens, cols, rows);
        let xs: Vec<(u16, u16)> = buf
            .area
            .positions()
            .filter(|p| buf[*p].symbol() == "X")
            .map(|p| (p.x, p.y))
            .collect();
        assert_eq!(
            xs.len(),
            usize::from(size.cols) * usize::from(size.rows),
            "{cols}x{rows}: the whole screen is drawn"
        );
        assert!(
            xs.iter().all(|&(x, y)| pane.contains(Position::new(x, y))),
            "{cols}x{rows}: only inside the pane"
        );
    }
}

#[test]
fn the_sidebar_collapses_below_60_columns() {
    let wide = layout(Rect::new(0, 0, 60, 20));
    let narrow = layout(Rect::new(0, 0, 59, 20));
    assert!(!wide.collapsed && narrow.collapsed);
    assert!(narrow.sidebar.width < wide.sidebar.width);
}

#[test]
fn the_pane_keeps_the_smallest_screen_width_and_the_sidebar_goes_first() {
    use shepherd_term::MIN_COLS;
    for cols in 0..=30 {
        let l = layout(Rect::new(0, 0, cols, 5));
        assert!(
            l.pane.width >= cols.min(MIN_COLS),
            "{cols}: pane {}",
            l.pane.width
        );
        assert_eq!(l.sidebar.width + l.pane.width, cols, "{cols}");
        if l.sidebar.width > 0 {
            assert!(l.pane.width >= MIN_COLS, "{cols}");
        }
    }
    // Tiny windows still get a screen the emulator can hold.
    assert_eq!(
        (pane_size(0, 0).cols, pane_size(0, 0).rows),
        (shepherd_term::MIN_COLS, shepherd_term::MIN_ROWS)
    );
}

#[test]
fn the_sidebar_scrolls_to_keep_the_selected_session_visible() {
    let ids: Vec<String> = (1..=15).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    // The newest is selected at the top; going back once wraps to the oldest, at the bottom of the list.
    c.user(Intent::PrevSession);
    let selected = c
        .client()
        .sessions()
        .iter()
        .find(|s| Some(&s.id) == c.client().selected())
        .map(|s| s.name.clone())
        .unwrap_or_default();
    let buf = buffer(c.client(), &Screens::new(), 80, 12);
    let side = layout(buf.area).sidebar;
    let row_of = |y: u16| -> String {
        (side.x..side.right() - 1)
            .map(|x| buf[(x, y)].symbol().to_owned())
            .collect()
    };
    let shown = (0..buf.area.height).find(|&y| row_of(y).trim_end().ends_with(&selected));
    let Some(y) = shown else {
        panic!("{selected} not shown: {buf:?}");
    };
    assert_ne!(buf[(side.x + 2, y)].bg, Color::Reset, "and highlighted");
}

#[test]
fn the_selected_row_is_highlighted_across_both_lines() {
    let (c, screens) = three_sessions();
    let buf = buffer(c.client(), &screens, 80, 24);
    let side = layout(buf.area).sidebar;
    let highlighted = |y: u16| buf[(side.x + 2, y)].bg != Color::Reset;
    // The newest session is at the top and selected.
    let rows: Vec<bool> = (1..7).map(highlighted).collect();
    assert_eq!(rows, [true, true, false, false, false, false]);
}

// ---------- the cursor ----------

#[test]
fn the_cursor_is_the_selected_sessions_cursor_in_the_pane() {
    let (c, screens) = three_sessions();
    let terminal = draw(c.client(), &screens, 80, 24);
    let pane = layout(Rect::new(0, 0, 80, 24)).pane;
    let cursor = screens[&SessionId::new("s3")].cursor();
    assert!(terminal.backend().cursor_visible());
    assert_eq!(
        terminal.backend().cursor_position(),
        Position::new(pane.x + cursor.col, pane.y + cursor.row)
    );
}

#[test]
fn the_cursor_is_hidden_when_the_session_hides_it() {
    let (c, mut screens) = three_sessions();
    if let Some(s) = screens.get_mut(&SessionId::new("s3")) {
        s.feed(b"\x1b[?25l");
    }
    let terminal = draw(c.client(), &screens, 80, 24);
    assert!(!terminal.backend().cursor_visible());
}

#[test]
fn the_cursor_is_hidden_after_the_prefix_and_under_a_popup() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::SetFocus(Focus::Command));
    assert!(
        !draw(c.client(), &screens, 80, 24)
            .backend()
            .cursor_visible()
    );
    c.user(Intent::SetFocus(Focus::Terminal));
    c.user(Intent::BeginClose);
    assert!(
        !draw(c.client(), &screens, 80, 24)
            .backend()
            .cursor_visible()
    );
}

#[test]
fn the_rename_popup_puts_the_cursor_after_the_name() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::BeginRename);
    let terminal = draw(c.client(), &screens, 80, 24);
    assert!(terminal.backend().cursor_visible());
    let at = terminal.backend().cursor_position();
    let buf = terminal.backend().buffer();
    let before: String = (at.x - 8..at.x)
        .map(|x| buf[(x, at.y)].symbol().to_owned())
        .collect();
    assert_eq!(before, "refactor");
}

// ---------- glyphs ----------

#[test]
fn status_glyphs_follow_the_documented_table() {
    use shepherd_core::client::ClientSession;
    use shepherd_core::session::Activity;
    use shepherd_testkit::builders::ts;
    use shepherd_tui::sidebar::glyph;

    let session = |activity: Activity, exited: bool| ClientSession {
        id: SessionId::new("s1"),
        name: "s1".into(),
        created: ts("2026-10-29T12:00:00Z"),
        exited,
        activity,
        attention: false,
    };
    let table = [
        (Activity::Starting, false, ("◌", Color::DarkGray)),
        (Activity::Working { tool: None }, false, ("●", Color::Green)),
        (
            Activity::NeedsPermission { title: "x".into() },
            false,
            ("▲", Color::Yellow),
        ),
        (Activity::NeedsInput, false, ("▲", Color::Yellow)),
        (Activity::Idle, false, ("○", Color::Reset)),
        (Activity::Exited { code: Some(0) }, false, ("✕", Color::Red)),
        // A PTY that ended is exited whatever the log last said.
        (Activity::Working { tool: None }, true, ("✕", Color::Red)),
        (Activity::Unknown, false, ("?", Color::DarkGray)),
    ];
    for (activity, exited, want) in table {
        assert_eq!(
            glyph(&session(activity.clone(), exited)),
            want,
            "{activity:?}"
        );
    }
}
