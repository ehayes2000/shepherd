//! `render`: the sidebar, the selected session's terminal, popups and help, drawn from client state.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::Event;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use serde_json::{Value, json};
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::{Cell, Client, Focus, Intent};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::WireError;
use shepherd_term::Modes;
use shepherd_term::Screen;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, Screens, cursor_style, layout, map_event, pane_size, render};

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

/// Tells the client the window is `cols` x `rows`, as the runtime does with the terminal's resize.
fn resized(c: &mut ClientDriver, cols: u16, rows: u16) {
    let window = Rect::new(0, 0, cols, rows);
    let event = Event::Resize(cols, rows);
    let mut pointer = Pointer::default();
    for intent in map_event(&event, c.client(), &Modes::default(), window, &mut pointer) {
        c.user(intent);
    }
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

/// Right-clicked on the first line of `docs` (under the three header lines), which isn't the selected session.
#[test]
fn context_menu_open() {
    let (mut c, screens) = three_sessions();
    c.user(Intent::OpenMenu {
        session: SessionId::new("s2"),
        at: Cell { col: 5, row: 5 },
    });
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

/// A Claude Code session `fix-login` whose agent reported `agent_session` and exited, right-clicked.
fn exited_claude_menu(agent_session: Option<&str>) -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host()
        .new_session_with("s1", shepherd_testkit::builders::claude_session_spec());
    c.host().rename("s1", "fix-login");
    c.host().agent_started("s1", "claude-code", agent_session);
    c.host().pty_exited("s1", Some(0));
    c.sync();
    c.user(Intent::OpenMenu {
        session: SessionId::new("s1"),
        at: Cell { col: 3, row: 3 },
    });
    c
}

#[test]
fn context_menu_on_an_exited_session_offers_resume() {
    let c = exited_claude_menu(Some("abc123"));
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 8));
}

#[test]
fn context_menu_with_resume_unavailable_says_why() {
    let c = exited_claude_menu(None);
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 8));
}

/// Clicked `+ New session` with three profiles, `codex` the default.
#[test]
fn profile_picker() {
    let (_, screens) = three_sessions();
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("claude", AgentKind::ClaudeCode),
            ("codex", AgentKind::Codex),
            ("shell", AgentKind::Shell),
        ],
        "codex",
    );
    c.host().new_session("s3");
    c.host().rename("s3", "refactor");
    c.sync();
    c.user(Intent::BeginNewSession);
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

/// The picker over twenty shell profiles in a short window, scrolled down three rows.
#[test]
fn profile_picker_scrolled() {
    let names: Vec<String> = (1..=20).map(|i| format!("profile-{i:02}")).collect();
    let profiles: Vec<(&str, AgentKind)> = names
        .iter()
        .map(|n| (n.as_str(), AgentKind::Shell))
        .collect();
    let mut c = ClientDriver::new();
    c.connect_with_profiles(&profiles, "profile-01");
    c.user(Intent::BeginNewSession);
    c.user(Intent::ScrollPicker { by: 3, shown: 6 });
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 10));
}

/// The picker with a default profile whose name is longer than the dialog has room for.
#[test]
fn profile_picker_with_a_long_name() {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            (
                "claude-with-a-very-long-profile-name",
                AgentKind::ClaudeCode,
            ),
            ("shell", AgentKind::Shell),
        ],
        "claude-with-a-very-long-profile-name",
    );
    c.user(Intent::BeginNewSession);
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 10));
}

/// The picker scrolled to its end in a short window, then drawn in a taller one: it shows more rows, and the last
/// profile still fills the bottom row.
#[test]
fn a_picker_scrolled_to_its_end_fills_a_taller_window_to_its_last_row() {
    let names: Vec<String> = (1..=30).map(|i| format!("profile-{i:02}")).collect();
    let profiles: Vec<(&str, AgentKind)> = names
        .iter()
        .map(|n| (n.as_str(), AgentKind::Shell))
        .collect();
    let mut c = ClientDriver::new();
    c.connect_with_profiles(&profiles, "profile-01");
    c.user(Intent::BeginNewSession);
    // A 10-row window shows 6 profiles; the end is 24 rows down.
    c.user(Intent::ScrollPicker { by: 100, shown: 6 });
    // A 24-row window shows 20: profile-11 to profile-30.
    let buf = buffer(c.client(), &Screens::new(), 80, 24);
    let last = position_of(&buf, "profile-30");
    let cancel = position_of(&buf, "[Cancel]");
    assert_eq!(
        last.y + 1,
        cancel.y,
        "no blank rows below the last: {buf:?}"
    );
    assert_eq!(position_of(&buf, "profile-11").y, last.y - 19, "{buf:?}");
}

/// Every kind of popup, each opened on a client of its own.
fn every_popup() -> Vec<(&'static str, ClientDriver)> {
    let mut popups = Vec::new();
    let opened = |intents: &[Intent]| {
        let (mut c, _) = three_sessions();
        for intent in intents {
            c.user(intent.clone());
        }
        c
    };
    popups.push(("help", opened(&[Intent::ShowHelp])));
    popups.push((
        "rename",
        opened(&[Intent::BeginRename, Intent::EditRename("api".into())]),
    ));
    popups.push(("close", opened(&[Intent::BeginClose])));
    popups.push((
        "menu",
        opened(&[Intent::OpenMenu {
            session: SessionId::new("s2"),
            at: Cell { col: 5, row: 4 },
        }]),
    ));
    popups.push(("menu with resume", exited_claude_menu(Some("abc123"))));
    popups.push(("menu with resume unavailable", exited_claude_menu(None)));
    let names: Vec<String> = (1..=12).map(|i| format!("profile-{i:02}")).collect();
    let profiles: Vec<(&str, AgentKind)> = names
        .iter()
        .map(|n| (n.as_str(), AgentKind::ClaudeCode))
        .collect();
    let mut picker = ClientDriver::new();
    picker.connect_with_profiles(&profiles, "profile-01");
    picker.user(Intent::BeginNewSession);
    popups.push(("profile picker", picker));
    let mut switcher = two_workspaces();
    switcher.user(Intent::BeginWorkspace);
    switcher.user(Intent::EditWorkspacePath("/src/new".into()));
    popups.push(("workspace switcher", switcher));
    let mut refused = two_workspaces();
    refused.user(Intent::BeginWorkspace);
    refused.user(Intent::Confirm);
    popups.push(("workspace switcher with an error", refused));
    let mut scrolled = workspaces_used(20);
    scrolled.host().select_workspace(LONG_PATH);
    scrolled.sync();
    scrolled.user(Intent::BeginWorkspace);
    scrolled.user(Intent::ScrollSwitcher { by: 3, shown: 5 });
    popups.push(("workspace switcher scrolled, with a long path", scrolled));
    // A client that knows its build, connected to a host that says none.
    let build = shepherd_core::wire::BuildId {
        version: "0.1.0".into(),
        digest: shepherd_core::wire::ExeDigest::from_bytes([1; 32]),
    };
    let mut restart = ClientDriver::from_client(
        Client::new(shepherd_core::ids::MachineId::new("m_client")).with_build(build),
    );
    restart.connect();
    popups.push(("restart dialog", restart));
    popups.push(("finder loading", opened(&[Intent::BeginFind])));
    popups.push((
        "finder with no results",
        opened(&[Intent::BeginFind, Intent::EditFind("zzz".into())]),
    ));
    let (mut found, _) = three_sessions();
    let asked = found.user(Intent::BeginFind);
    found.answer(
        &asked,
        Ok(json!({ "worktrees": [{ "path": "/wt/app/old-idea", "branch": "old-idea" }] })),
    );
    popups.push(("finder with a worktree", found.clone()));
    found.user(Intent::FinderChoose(
        shepherd_core::client::FinderItem::Worktree(shepherd_core::client::Orphan {
            path: "/wt/app/old-idea".into(),
            branch: Some("old-idea".into()),
        }),
    ));
    popups.push(("offer to start a session in a worktree", found));
    popups
}

#[test]
fn every_popup_draws_in_any_window_however_small() {
    let mut sizes: Vec<(u16, u16)> = (0..=6).flat_map(|w| (0..=6).map(move |h| (w, h))).collect();
    sizes.extend([
        (80, 0),
        (80, 1),
        (3, 24),
        (10, 1),
        (13, 7),
        (41, 5),
        (57, 3),
    ]);
    let mut panicked = Vec::new();
    for (name, c) in every_popup() {
        assert!(c.client().popup().is_some(), "{name}");
        for &(cols, rows) in &sizes {
            let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buffer(c.client(), &Screens::new(), cols, rows)
            }));
            if drawn.is_err() {
                panicked.push(format!("{name} at {cols}x{rows}"));
            }
        }
    }
    assert_eq!(panicked, Vec::<String>::new());
}

/// `three_sessions` showing a notice: a refused rename.
fn with_notice() -> (ClientDriver, Screens) {
    let (mut c, screens) = three_sessions();
    c.user(Intent::BeginRename);
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(WireError::UnknownSession {
            session: SessionId::new("s3"),
        }),
    );
    (c, screens)
}

/// The footer with a notice: it takes the whole width and the `Detach` and `?` buttons hide.
#[test]
fn footer_with_a_notice() {
    let (c, screens) = with_notice();
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

/// The footer once the notice is cleared by the next user action: the buttons are back.
#[test]
fn footer_after_the_notice_clears() {
    let (mut c, screens) = with_notice();
    c.user(Intent::Dismiss);
    assert_eq!(c.client().notice(), None);
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 80, 24));
}

#[test]
fn the_hidden_footer_buttons_cant_be_clicked_while_a_notice_shows() {
    let (mut c, _) = with_notice();
    let window = Rect::new(0, 0, 80, 24);
    let detach = {
        let (mut c, screens) = with_notice();
        c.user(Intent::Dismiss);
        position_of(&buffer(c.client(), &screens, 80, 24), "Detach")
    };
    // `Detach ?`: the help button is a blank column after it.
    let help = Position::new(detach.x + 7, detach.y);
    let hits = |c: &ClientDriver| {
        [detach, help].map(|at| shepherd_tui::hit(&layout(window), c.client(), at.x, at.y))
    };
    assert!(
        !hits(&c).iter().any(|hit| matches!(
            hit,
            Some(shepherd_tui::Target::Detach | shepherd_tui::Target::Help)
        )),
        "{:?}",
        hits(&c)
    );
    c.user(Intent::Dismiss);
    assert_eq!(
        hits(&c),
        [
            Some(shepherd_tui::Target::Detach),
            Some(shepherd_tui::Target::Help)
        ]
    );
}

/// Four sessions: `api` asked permission and `docs` went idle while unselected (both marked), `tests` went idle and
/// was looked at since (not marked), and `refactor` (newest, selected) is working.
fn attention_sessions() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3", "s4"]);
    for (id, name) in [
        ("s1", "api"),
        ("s2", "docs"),
        ("s3", "tests"),
        ("s4", "refactor"),
    ] {
        c.host().rename(id, name);
    }
    prompt(&mut c, "s3");
    idle(&mut c, "s3");
    c.sync();
    c.user(Intent::SelectSession(SessionId::new("s3")));
    c.user(Intent::SelectSession(SessionId::new("s4")));
    prompt(&mut c, "s1");
    permission(&mut c, "s1");
    prompt(&mut c, "s2");
    idle(&mut c, "s2");
    prompt(&mut c, "s4");
    c.sync();
    c
}

#[test]
fn sidebar_with_attention_markers() {
    let c = attention_sessions();
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 12));
}

#[test]
fn clicking_a_marked_row_selects_it_and_clears_its_mark() {
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut c = attention_sessions();
    let buf = buffer(c.client(), &Screens::new(), 80, 12);
    let api = position_of(&buf, "api");
    // The mark is the row's first cell.
    let mark = Position::new(0, api.y);
    assert_eq!(buf[mark].symbol(), "▌");
    assert_eq!(buf[mark].fg, Color::Yellow);
    let window = Rect::new(0, 0, 80, 12);
    let mut pointer = Pointer::default();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let event = Event::Mouse(MouseEvent {
            kind,
            column: mark.x,
            row: mark.y,
            modifiers: KeyModifiers::NONE,
        });
        for intent in map_event(&event, c.client(), &Modes::default(), window, &mut pointer) {
            c.user(intent);
        }
    }
    assert_eq!(c.client().selected(), Some(&SessionId::new("s1")));
    let buf = buffer(c.client(), &Screens::new(), 80, 12);
    assert_eq!(buf[mark].symbol(), " ", "no mark once selected");
}

/// Where `text` starts in `buf`. Panics if it isn't drawn.
fn position_of(buf: &Buffer, text: &str) -> Position {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    buf.area
        .positions()
        .find(|p| {
            chars.iter().enumerate().all(|(i, ch)| {
                let x = p.x + i as u16;
                x < buf.area.right() && buf[(x, p.y)].symbol() == ch
            })
        })
        .unwrap_or_else(|| panic!("{text:?} not drawn: {buf:?}"))
}

#[test]
fn narrow_terminal_collapses_the_sidebar_to_glyphs() {
    let (c, screens) = three_sessions();
    insta::assert_debug_snapshot!(buffer(c.client(), &screens, 40, 12));
}

/// Fifteen sessions in a short window, scrolled down by the wheel (`ScrollSidebar`) past the selected newest one.
#[test]
fn a_scrolled_session_list() {
    let ids: Vec<String> = (1..=15).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    resized(&mut c, 80, 12);
    c.user(Intent::ScrollSidebar(4));
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 12));
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
    resized(&mut c, 80, 12);
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
    // Left of each row's close button.
    let row_of = |y: u16| -> String {
        (side.x..side.right() - 2)
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
fn a_taller_window_than_the_client_was_told_still_fills_the_list_to_its_end() {
    let ids: Vec<String> = (1..=15).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    // Told 4 rows fit, scrolled to the end (rows 11..15); drawn where 10 fit, before the resize arrives.
    resized(&mut c, 80, 12);
    c.user(Intent::ScrollSidebar(100));
    let buf = buffer(c.client(), &Screens::new(), 80, 24);
    let side = layout(buf.area).sidebar;
    // Left of each row's close button.
    let row_of = |y: u16| -> String {
        (side.x..side.right() - 2)
            .map(|x| buf[(x, y)].symbol().to_owned())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    // The last 10 sessions, newest first: shell-10 under the three header lines, down to shell-1 on the last row that
    // fits above the footer.
    assert!(row_of(3).ends_with("shell-10"), "{buf:?}");
    assert!(row_of(21).ends_with("shell-1"), "{buf:?}");
}

#[test]
fn the_selected_row_is_highlighted_across_both_lines() {
    let (c, screens) = three_sessions();
    let buf = buffer(c.client(), &screens, 80, 24);
    let side = layout(buf.area).sidebar;
    let highlighted = |y: u16| buf[(side.x + 2, y)].bg != Color::Reset;
    // The newest session is at the top, under the three header lines, and selected.
    let rows: Vec<bool> = (3..9).map(highlighted).collect();
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
fn the_collapsed_sidebar_keeps_its_buttons_while_a_notice_shows() {
    let (c, screens) = with_notice();
    let window = Rect::new(0, 0, 40, 12);
    let buf = buffer(c.client(), &screens, 40, 12);
    let detach = position_of(&buf, "⏏");
    // `?` in the glyph column, on the last line.
    let help = Position::new(detach.x, detach.y + 1);
    assert_eq!(buf[help].symbol(), "?");
    assert_eq!(
        [detach, help].map(|at| shepherd_tui::hit(&layout(window), c.client(), at.x, at.y)),
        [
            Some(shepherd_tui::Target::Detach),
            Some(shepherd_tui::Target::Help)
        ]
    );
}

/// `three_sessions` after the selected session sent `decscusr` (`CSI Ps SP q`).
fn with_cursor_style(decscusr: &[u8]) -> (ClientDriver, Screens) {
    let (c, mut screens) = three_sessions();
    if let Some(s) = screens.get_mut(&SessionId::new("s3")) {
        s.feed(decscusr);
    }
    (c, screens)
}

#[test]
fn the_cursor_takes_the_selected_sessions_shape_and_blink() {
    use ratatui::crossterm::cursor::SetCursorStyle;
    let table = [
        (&b"\x1b[1 q"[..], SetCursorStyle::BlinkingBlock),
        (b"\x1b[3 q", SetCursorStyle::BlinkingUnderScore),
        (b"\x1b[4 q", SetCursorStyle::SteadyUnderScore),
        (b"\x1b[5 q", SetCursorStyle::BlinkingBar),
        (b"\x1b[6 q", SetCursorStyle::SteadyBar),
    ];
    for (decscusr, want) in table {
        let (c, screens) = with_cursor_style(decscusr);
        assert_eq!(
            cursor_style(c.client(), &screens),
            want,
            "{}",
            String::from_utf8_lossy(decscusr)
        );
    }
}

#[test]
fn a_session_that_never_set_its_cursor_leaves_the_users_own() {
    let (c, screens) = three_sessions();
    assert_eq!(
        cursor_style(c.client(), &screens),
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    // Back to the default (`0`), the same.
    let (c, screens) = with_cursor_style(b"\x1b[5 q\x1b[0 q");
    assert_eq!(
        cursor_style(c.client(), &screens),
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
}

#[test]
fn a_cursor_that_isnt_the_sessions_has_the_users_own_shape() {
    use ratatui::crossterm::cursor::SetCursorStyle;
    let (mut c, screens) = with_cursor_style(b"\x1b[5 q");
    c.user(Intent::BeginRename);
    assert_eq!(
        cursor_style(c.client(), &screens),
        SetCursorStyle::DefaultUserShape
    );
    c.user(Intent::Dismiss);
    c.user(Intent::SetFocus(Focus::Command));
    assert_eq!(
        cursor_style(c.client(), &screens),
        SetCursorStyle::DefaultUserShape
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
        resume: Ok(()),
        workspace: None,
        elsewhere: false,
        creating_worktree: false,
        failed: None,
        warning: None,
        worktree: None,
        branch: None,
        removing_worktree: false,
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
        // Not `?`: that is the help button, in the same column of the collapsed sidebar.
        (Activity::Unknown, false, ("·", Color::DarkGray)),
    ];
    for (activity, exited, want) in table {
        assert_eq!(
            glyph(&session(activity.clone(), exited)),
            want,
            "{activity:?}"
        );
    }
}

// ---------- workspaces ----------

/// `login` was started in `/src/api`; then `/src/shepherd` was selected and `docs` and `refactor` (newest, selected)
/// started there. Each runs in its worktree.
fn two_workspaces() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/api");
    c.host().new_session("s1");
    c.host().rename("s1", "login");
    c.host().select_workspace("/src/shepherd");
    c.host().new_session("s2");
    c.host().rename("s2", "docs");
    c.host().new_session("s3");
    c.host().rename("s3", "refactor");
    for id in ["s1", "s2", "s3"] {
        c.host().worktree_ready(id);
    }
    c.sync();
    c
}

#[test]
fn a_session_making_its_worktree_and_one_whose_worktree_failed() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/shepherd");
    c.host().new_session("s1");
    c.host().rename("s1", "docs");
    c.host().new_session("s2");
    c.host().rename("s2", "refactor");
    let error = shepherd_core::ports::WorktreeError::FetchFailed(
        "could not resolve host: github.com".to_owned(),
    );
    c.host().worktree_result("s2", Err(error));
    c.sync();
    // The reason is cut to the row's width.
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 8));
}

#[test]
fn a_session_running_its_hooks_and_one_whose_hook_failed() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/shepherd");
    c.host().new_session("s1");
    c.host().rename("s1", "docs");
    c.host().worktree_ready_with_hooks("s1", &["npm ci"]);
    c.host().new_session("s2");
    c.host().rename("s2", "refactor");
    c.host()
        .worktree_ready_with_hooks("s2", &["cp \"$SHEPHERD_REPO/.envrc\" ."]);
    c.host()
        .hook_exited("s2", "cp \"$SHEPHERD_REPO/.envrc\" .", 1);
    c.sync();
    // The warning is cut to the row's width.
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 8));
}

#[test]
fn the_workspace_heads_the_sidebar_and_other_workspaces_sessions_are_dimmed() {
    let c = two_workspaces();
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 12));
}

#[test]
fn workspace_switcher() {
    let mut c = two_workspaces();
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/src/new".into()));
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 24));
}

#[test]
fn workspace_switcher_with_a_refused_path() {
    let mut c = two_workspaces();
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(WireError::InvalidParams("/tmp/x is not a repo".into())),
    );
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 80, 24));
}

/// A host that has used `n` workspaces, `/src/repo-01` (the oldest) to `/src/repo-<n>` (current).
fn workspaces_used(n: usize) -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    for i in 1..=n {
        c.host().select_workspace(&format!("/src/repo-{i:02}"));
    }
    c.sync();
    c
}

/// A path longer than the switcher has room for.
const LONG_PATH: &str = "/Users/someone/a/deeply/nested/directory/full/of/many/projects/shepherd";

/// The switcher over twenty workspaces in a short window, scrolled down three rows.
#[test]
fn workspace_switcher_scrolled() {
    let mut c = workspaces_used(20);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::ScrollSwitcher { by: 3, shown: 5 });
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 12));
}

/// The switcher whose current workspace's path is longer than the dialog has room for.
#[test]
fn workspace_switcher_with_a_long_path() {
    let mut c = workspaces_used(1);
    c.host().select_workspace(LONG_PATH);
    c.sync();
    c.user(Intent::BeginWorkspace);
    insta::assert_debug_snapshot!(buffer(c.client(), &Screens::new(), 60, 12));
}

/// The switcher scrolled to its end in a short window, then drawn in a taller one: it shows more rows, and the oldest
/// workspace still fills the bottom row of the list.
#[test]
fn a_switcher_scrolled_to_its_end_fills_a_taller_window_to_its_last_row() {
    let mut c = workspaces_used(30);
    c.user(Intent::BeginWorkspace);
    // A 12-row window shows 5 workspaces; the end is 25 rows down.
    c.user(Intent::ScrollSwitcher { by: 100, shown: 5 });
    // A 24-row window shows 17: repo-17 to repo-01.
    let buf = buffer(c.client(), &Screens::new(), 80, 24);
    let last = position_of(&buf, "/src/repo-01");
    let prompt = position_of(&buf, "Open a repo by path:");
    assert_eq!(
        last.y + 1,
        prompt.y,
        "no blank rows below the last: {buf:?}"
    );
    assert_eq!(position_of(&buf, "/src/repo-17").y, last.y - 16, "{buf:?}");
}

#[test]
fn the_workspace_switcher_puts_the_cursor_after_the_path() {
    let mut c = two_workspaces();
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/src".into()));
    let terminal = draw(c.client(), &Screens::new(), 80, 24);
    assert!(terminal.backend().cursor_visible());
    let buf = terminal.backend().buffer().clone();
    // The listed workspaces go on past `/src`; the typed path doesn't.
    let typed = position_of(&buf, "/src ");
    assert_eq!(
        terminal.backend().cursor_position(),
        Position::new(typed.x + 4, typed.y),
        "{buf:?}"
    );
}

// ---------- display width ----------

/// Line `y` of `buf` as the terminal shows it: the cells a wide symbol covers are left out.
fn line_text(buf: &Buffer, y: u16) -> String {
    let mut text = String::new();
    let mut x = buf.area.left();
    while x < buf.area.right() {
        let symbol = buf[(x, y)].symbol();
        text.push_str(symbol);
        x += u16::try_from(columns(symbol).max(1)).unwrap_or(1);
    }
    text
}

/// The first line of `buf` showing `text`, as the terminal shows it. Panics if none does.
fn line_with(buf: &Buffer, text: &str) -> (u16, String) {
    (buf.area.top()..buf.area.bottom())
        .map(|y| (y, line_text(buf, y)))
        .find(|(_, line)| line.contains(text))
        .unwrap_or_else(|| panic!("{text:?} not drawn: {buf:?}"))
}

/// How many columns `text` takes.
#[allow(clippy::disallowed_methods)] // Measures text; never draws it.
fn columns(text: &str) -> usize {
    ratatui::text::Span::raw(text).width()
}

/// A path too wide for the switcher, though not too many characters for it.
const CJK_PATH: &str = "/ユーザー/誰か/とても/深い/ディレクトリ/羊飼い";

#[test]
fn a_cjk_path_too_wide_for_the_switcher_keeps_the_repo_name_and_the_mark() {
    let mut c = workspaces_used(1);
    c.host().select_workspace(CJK_PATH);
    c.sync();
    c.user(Intent::BeginWorkspace);
    let buf = buffer(c.client(), &Screens::new(), 60, 12);
    let (_, row) = line_with(&buf, "/羊飼い");
    assert!(row.contains("│ …ザー/"), "cut from the left: {row:?}");
    assert!(row.contains("/羊飼い  (current)│"), "{row:?}");
}

#[test]
fn an_emoji_path_too_wide_for_the_switcher_keeps_the_repo_name_and_the_mark() {
    let path = format!("/src/{}/flock", "🐑".repeat(30));
    let mut c = workspaces_used(1);
    c.host().select_workspace(&path);
    c.sync();
    c.user(Intent::BeginWorkspace);
    let buf = buffer(c.client(), &Screens::new(), 60, 12);
    let (_, row) = line_with(&buf, "/flock");
    assert!(row.contains("│ …🐑"), "cut from the left: {row:?}");
    assert!(row.contains("🐑/flock  (current)"), "{row:?}");
}

#[test]
fn a_cjk_profile_name_too_wide_for_the_picker_keeps_the_agent_and_the_mark() {
    let name = "羊飼いのとても長いプロファイル名前";
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[(name, AgentKind::ClaudeCode), ("shell", AgentKind::Shell)],
        name,
    );
    c.user(Intent::BeginNewSession);
    let buf = buffer(c.client(), &Screens::new(), 60, 10);
    let (_, row) = line_with(&buf, "羊飼い");
    assert!(row.contains("…  claude-code  (default)│"), "{row:?}");
}

/// The agent column starts in the same column on every row, however wide the names before it.
#[test]
fn the_pickers_agent_column_lines_up_after_wide_names() {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("羊飼い", AgentKind::ClaudeCode),
            ("shell", AgentKind::Shell),
        ],
        "shell",
    );
    c.user(Intent::BeginNewSession);
    let buf = buffer(c.client(), &Screens::new(), 60, 10);
    let (_, wide) = line_with(&buf, "羊飼い");
    let (_, narrow) = line_with(&buf, "(default)");
    let agent = |row: &str, at: Option<usize>| columns(&row[..at.unwrap_or_default()]);
    assert_eq!(
        agent(&wide, wide.find("claude-code")),
        agent(&narrow, narrow.rfind("shell")),
        "{buf:?}"
    );
}

/// A name of skin-toned emoji, each one grapheme of two characters, cut to fit: no emoji is drawn in part. (Not a
/// ZWJ family: display-safe text drops the joiners, F35.)
#[test]
fn a_cut_profile_name_never_splits_a_grapheme() {
    let family = "👍🏽";
    let name = family.repeat(20);
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            (name.as_str(), AgentKind::Shell),
            ("claude", AgentKind::ClaudeCode),
        ],
        name.as_str(),
    );
    c.user(Intent::BeginNewSession);
    let buf = buffer(c.client(), &Screens::new(), 60, 10);
    let (y, row) = line_with(&buf, "(default)");
    assert!(row.contains("🏽…  shell"), "{row:?}");
    assert!(row.contains("(default)│"), "{row:?}");
    for x in buf.area.left()..buf.area.right() {
        let symbol = buf[(x, y)].symbol();
        let in_a_family = symbol.chars().any(|c| matches!(c, '👍' | '🏽'));
        assert!(
            symbol == family || !in_a_family,
            "{symbol:?} is part of a family: {row:?}"
        );
    }
}

/// A typed path too wide for the input keeps its end in view, the cursor after it.
#[test]
fn a_wide_typed_path_keeps_its_end_in_view() {
    let mut c = two_workspaces();
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath(format!(
        "{CJK_PATH}/もっと/終わり"
    )));
    let terminal = draw(c.client(), &Screens::new(), 80, 24);
    let buf = terminal.backend().buffer().clone();
    let (y, _) = line_with(&buf, "/終わり");
    let cursor = terminal.backend().cursor_position();
    assert_eq!(cursor.y, y, "the cursor is on the input: {buf:?}");
    assert_eq!(buf[(cursor.x - 2, cursor.y)].symbol(), "り", "{buf:?}");
}

/// The sidebar's workspace button is as wide as its label in columns, so a wide name is drawn whole.
#[test]
fn a_wide_workspace_name_heads_the_sidebar_whole() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/羊🐑群れ");
    c.sync();
    let buf = buffer(c.client(), &Screens::new(), 80, 8);
    assert!(line_text(&buf, 1).starts_with(" ⌂ 羊🐑群れ "), "{buf:?}");
}

/// A session with a wide name, from another workspace whose wide name is too long for the row: the name is drawn
/// with its `×`, and the second line ends in `…` at the border.
#[test]
fn sidebar_rows_with_wide_names() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host()
        .select_workspace("/src/羊飼いのとても長いリポジトリ");
    c.host().new_session("s1");
    c.host().rename("s1", "🐑 羊");
    c.host().worktree_ready("s1");
    c.host().select_workspace("/src/shepherd");
    c.sync();
    let buf = buffer(c.client(), &Screens::new(), 80, 8);
    let (_, name) = line_with(&buf, "🐑 羊");
    assert!(name.contains("×│"), "{name:?}");
    let (_, detail) = line_with(&buf, "in 羊");
    assert!(detail.contains("…│"), "{detail:?}");
}
