//! Clicking and scrolling: `hit` finds the control under a cell, and `map_event` turns presses, releases and the wheel
//! into intents. Every test clicks where the control is drawn, found in the rendered buffer.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::{Cell, Focus, Intent, MenuItem, Popup};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::InputKind;
use shepherd_term::{Modes, MouseEncoding, MouseMode, MouseTracking};
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, render};

const WIDE: Rect = Rect::new(0, 0, 80, 24);
const SHORT: Rect = Rect::new(0, 0, 80, 12);
const NARROW: Rect = Rect::new(0, 0, 40, 12);

/// A connected client with `n` sessions named `name-01` (oldest) to `name-<n>` (newest, selected), told the size of
/// `window`.
fn sessions(n: usize, window: Rect) -> ClientDriver {
    let ids: Vec<String> = (1..=n).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    for i in 1..=n {
        c.host().rename(&format!("s{i}"), &format!("name-{i:02}"));
    }
    c.sync();
    let resize = Event::Resize(window.width, window.height);
    for intent in map_event(
        &resize,
        c.client(),
        &Modes::default(),
        window,
        &mut Pointer::default(),
    ) {
        c.user(intent);
    }
    c
}

fn buffer(c: &ClientDriver, window: Rect) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(window.width, window.height)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(c.client(), &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

/// Where `text` starts in `buf`, reading cell by cell.
fn find(buf: &Buffer, text: &str) -> Option<Position> {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    buf.area.positions().find(|p| {
        chars.iter().enumerate().all(|(i, ch)| {
            let x = p.x + i as u16;
            x < buf.area.right() && buf[(x, p.y)].symbol() == ch
        })
    })
}

fn at(buf: &Buffer, text: &str) -> Position {
    find(buf, text).unwrap_or_else(|| panic!("{text:?} not drawn: {buf:?}"))
}

fn session(id: &str) -> Option<Target> {
    Some(Target::Session(SessionId::new(id)))
}

fn hit_at(c: &ClientDriver, window: Rect, p: Position) -> Option<Target> {
    hit(&layout(window), c.client(), p.x, p.y)
}

fn mouse(kind: MouseEventKind, p: Position) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: p.x,
        row: p.y,
        modifiers: KeyModifiers::NONE,
    })
}

const DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
const UP: MouseEventKind = MouseEventKind::Up(MouseButton::Left);

/// What each event means, in order, with one pointer, and the intents applied to the client as they come.
fn events_with(
    c: &mut ClientDriver,
    window: Rect,
    modes: Modes,
    events: &[Event],
) -> Vec<Vec<Intent>> {
    let mut pointer = Pointer::default();
    events
        .iter()
        .map(|event| {
            let intents = map_event(event, c.client(), &modes, window, &mut pointer);
            for intent in &intents {
                c.user(intent.clone());
            }
            intents
        })
        .collect()
}

fn events(c: &mut ClientDriver, window: Rect, events: &[Event]) -> Vec<Vec<Intent>> {
    events_with(c, window, Modes::default(), events)
}

/// A press and a release at `p`.
fn click(c: &mut ClientDriver, window: Rect, p: Position) -> Vec<Vec<Intent>> {
    events(c, window, &[mouse(DOWN, p), mouse(UP, p)])
}

fn sgr_mouse() -> Modes {
    Modes {
        mouse: MouseMode {
            tracking: MouseTracking::Click,
            encoding: MouseEncoding::Sgr,
        },
        ..Modes::default()
    }
}

fn right(p: Position, by: u16) -> Position {
    Position::new(p.x + by, p.y)
}

fn below(p: Position, by: u16) -> Position {
    Position::new(p.x, p.y + by)
}

// ---------- hit ----------

#[test]
fn the_new_session_button_is_hit_across_its_label_and_nowhere_else() {
    let c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    let label = "+ New session";
    let start = at(&buf, label);
    let end = right(start, label.chars().count() as u16 - 1);
    assert_eq!(hit_at(&c, WIDE, start), Some(Target::NewSession));
    assert_eq!(hit_at(&c, WIDE, end), Some(Target::NewSession));
    assert_eq!(hit_at(&c, WIDE, Position::new(start.x - 1, start.y)), None);
    assert_eq!(hit_at(&c, WIDE, right(end, 1)), None);
    assert_ne!(hit_at(&c, WIDE, below(start, 1)), Some(Target::NewSession));
}

#[test]
fn a_session_row_is_hit_on_both_its_lines_and_not_beyond() {
    let c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    // Newest first: name-02 (s2) on top, then name-01 (s1).
    let top = at(&buf, "name-02");
    let second = at(&buf, "name-01");
    assert_eq!(hit_at(&c, WIDE, top), session("s2"));
    assert_eq!(hit_at(&c, WIDE, below(top, 1)), session("s2"));
    // The line above the first row is the header, with the button over the name's column.
    assert_eq!(
        hit_at(&c, WIDE, Position::new(top.x, top.y - 1)),
        Some(Target::NewSession)
    );
    assert_eq!(second, below(top, 2));
    assert_eq!(hit_at(&c, WIDE, second), session("s1"));
    assert_eq!(hit_at(&c, WIDE, below(second, 1)), session("s1"));
    assert_eq!(hit_at(&c, WIDE, below(second, 2)), None);
}

#[test]
fn a_session_row_is_hit_across_the_sidebar_but_not_on_its_border() {
    let c = sessions(1, WIDE);
    let buf = buffer(&c, WIDE);
    let row = at(&buf, "name-01");
    let side = layout(WIDE).sidebar;
    let border = side.right() - 1;
    assert_eq!(buf[(border, row.y)].symbol(), "│");
    assert_eq!(
        hit_at(&c, WIDE, Position::new(side.x, row.y)),
        session("s1")
    );
    // The last cell of the first line is the close button; the second line is the row to the border.
    assert_eq!(
        hit_at(&c, WIDE, Position::new(border - 2, row.y)),
        session("s1")
    );
    assert_eq!(
        hit_at(&c, WIDE, Position::new(border - 1, row.y + 1)),
        session("s1")
    );
    assert_eq!(hit_at(&c, WIDE, Position::new(border, row.y)), None);
}

#[test]
fn the_footer_and_empty_sidebar_space_hit_nothing() {
    let c = sessions(1, WIDE);
    let buf = buffer(&c, WIDE);
    let footer = at(&buf, "ctrl-b ? help");
    assert_eq!(hit_at(&c, WIDE, footer), None);
    assert_eq!(
        hit_at(&c, WIDE, Position::new(footer.x, footer.y - 1)),
        None
    );
}

#[test]
fn the_pane_is_the_terminal_from_its_first_column() {
    let c = sessions(1, WIDE);
    let pane = layout(WIDE).pane;
    let first = Position::new(pane.x, pane.y);
    assert_eq!(hit_at(&c, WIDE, first), Some(Target::Terminal));
    assert_eq!(
        hit_at(&c, WIDE, Position::new(pane.right() - 1, pane.bottom() - 1)),
        Some(Target::Terminal)
    );
    assert_ne!(
        hit_at(&c, WIDE, Position::new(pane.x - 1, pane.y)),
        Some(Target::Terminal)
    );
}

#[test]
fn a_scrolled_list_is_hit_where_its_rows_are_drawn() {
    let mut c = sessions(15, SHORT);
    let wheel = mouse(MouseEventKind::ScrollDown, Position::new(2, 3));
    events(&mut c, SHORT, &[wheel.clone(), wheel.clone(), wheel]);
    let buf = buffer(&c, SHORT);
    assert!(find(&buf, "name-15").is_none(), "scrolled past the top");
    // Three down from the newest: name-12 is the first row.
    let first = at(&buf, "name-12");
    assert_eq!(hit_at(&c, SHORT, first), session("s12"));
    assert_eq!(
        hit_at(&c, SHORT, Position::new(first.x, first.y - 1)),
        Some(Target::NewSession)
    );
    // Four rows fit under the three header lines: name-12 down to name-09.
    let last = at(&buf, "name-09");
    assert_eq!(hit_at(&c, SHORT, below(last, 1)), session("s9"));
    // The line below the last row is the footer.
    assert_eq!(hit_at(&c, SHORT, below(last, 2)), None);
}

#[test]
fn the_collapsed_sidebar_is_hit_on_its_glyphs() {
    let c = sessions(3, NARROW);
    let buf = buffer(&c, NARROW);
    let plus = at(&buf, "+");
    assert_eq!(hit_at(&c, NARROW, plus), Some(Target::NewSession));
    // Each session is a starting glyph, newest first.
    let glyphs: Vec<u16> = (0..NARROW.height)
        .filter(|&y| buf[(plus.x, y)].symbol() == "◌")
        .collect();
    assert_eq!(glyphs.len(), 3, "{buf:?}");
    for (y, id) in glyphs.iter().zip(["s3", "s2", "s1"]) {
        assert_eq!(hit_at(&c, NARROW, Position::new(plus.x, *y)), session(id));
        assert_eq!(
            hit_at(&c, NARROW, Position::new(plus.x, y + 1)),
            session(id)
        );
    }
}

// ---------- clicks ----------

#[test]
fn clicking_new_session_begins_one_on_release() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    assert_eq!(
        click(&mut c, WIDE, button),
        [vec![], vec![Intent::BeginNewSession]]
    );
}

#[test]
fn a_release_on_another_control_than_the_press_does_nothing() {
    let mut c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    let (button, top, second) = (
        at(&buf, "+ New session"),
        at(&buf, "name-02"),
        at(&buf, "name-01"),
    );
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(DOWN, button),
            mouse(UP, top),
            mouse(DOWN, top),
            mouse(UP, second),
        ],
    );
    assert!(out.iter().all(Vec::is_empty), "{out:?}");
}

#[test]
fn a_press_and_release_on_one_row_count_even_on_different_lines_of_it() {
    let mut c = sessions(2, WIDE);
    let second = at(&buffer(&c, WIDE), "name-01");
    let out = events(
        &mut c,
        WIDE,
        &[mouse(DOWN, second), mouse(UP, below(second, 1))],
    );
    assert_eq!(
        out[1],
        [
            Intent::SelectSession(SessionId::new("s1")),
            Intent::SetFocus(Focus::Terminal)
        ]
    );
}

#[test]
fn a_release_without_a_press_does_nothing() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    assert_eq!(events(&mut c, WIDE, &[mouse(UP, button)]), [vec![]]);
}

#[test]
fn clicking_the_selected_row_only_focuses_the_terminal() {
    let mut c = sessions(2, WIDE);
    let top = at(&buffer(&c, WIDE), "name-02");
    assert_eq!(
        click(&mut c, WIDE, top),
        [vec![], vec![Intent::SetFocus(Focus::Terminal)]]
    );
}

#[test]
fn clicking_empty_sidebar_space_does_nothing() {
    let mut c = sessions(1, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    assert_eq!(click(&mut c, WIDE, below(row, 3)), [vec![], vec![]]);
}

#[test]
fn the_middle_button_on_a_row_does_nothing() {
    let mut c = sessions(2, WIDE);
    let second = at(&buffer(&c, WIDE), "name-01");
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(MouseEventKind::Down(MouseButton::Middle), second),
            mouse(MouseEventKind::Up(MouseButton::Middle), second),
        ],
    );
    assert_eq!(out, [vec![], vec![]]);
}

#[test]
fn a_click_on_a_control_under_a_popup_only_dismisses_the_popup() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    c.user(Intent::ShowHelp);
    assert_eq!(click(&mut c, WIDE, button), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

// ---------- the wheel ----------

#[test]
fn the_wheel_over_the_sidebar_scrolls_it() {
    let mut c = sessions(15, SHORT);
    let row = at(&buffer(&c, SHORT), "name-15");
    let out = events_with(
        &mut c,
        SHORT,
        sgr_mouse(),
        &[
            mouse(MouseEventKind::ScrollDown, row),
            mouse(MouseEventKind::ScrollUp, row),
        ],
    );
    assert_eq!(
        out,
        [
            vec![Intent::ScrollSidebar(1)],
            vec![Intent::ScrollSidebar(-1)]
        ]
    );
}

#[test]
fn rows_below_the_fold_are_reached_by_wheel_then_click() {
    let mut c = sessions(15, SHORT);
    assert!(
        find(&buffer(&c, SHORT), "name-01").is_none(),
        "starts hidden"
    );
    let over = Position::new(2, 4);
    let wheel: Vec<Event> = (0..20)
        .map(|_| mouse(MouseEventKind::ScrollDown, over))
        .collect();
    events(&mut c, SHORT, &wheel);
    let oldest = at(&buffer(&c, SHORT), "name-01");
    click(&mut c, SHORT, oldest);
    assert_eq!(c.client().selected(), Some(&SessionId::new("s1")));
    // Selecting it didn't move the list out from under the pointer.
    assert_eq!(find(&buffer(&c, SHORT), "name-01"), Some(oldest));
}

// ---------- the pane ----------

#[test]
fn a_click_in_an_untracked_pane_focuses_the_terminal_and_sends_nothing() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::SetFocus(Focus::Command));
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.x + 3, 2);
    assert_eq!(
        click(&mut c, WIDE, p),
        [vec![], vec![Intent::SetFocus(Focus::Terminal)]]
    );
}

#[test]
fn the_wheel_in_an_untracked_pane_is_not_sent() {
    let mut c = sessions(1, WIDE);
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.x + 3, 2);
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(MouseEventKind::ScrollUp, p),
            mouse(MouseEventKind::ScrollDown, p),
        ],
    );
    assert_eq!(out, [vec![], vec![]]);
}

#[test]
fn a_tracked_pane_gets_the_click_encoded() {
    let mut c = sessions(1, WIDE);
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.x + 3, 2);
    let out = events_with(&mut c, WIDE, sgr_mouse(), &[mouse(DOWN, p), mouse(UP, p)]);
    assert_eq!(
        out,
        [
            vec![Intent::Input(InputKind::Mouse, b"\x1b[<0;4;3M".to_vec())],
            vec![Intent::Input(InputKind::Mouse, b"\x1b[<0;4;3m".to_vec())]
        ]
    );
}

#[test]
fn a_tracked_pane_after_the_prefix_is_focused_by_a_click() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::SetFocus(Focus::Command));
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.x + 3, 2);
    let out = events_with(&mut c, WIDE, sgr_mouse(), &[mouse(DOWN, p), mouse(UP, p)]);
    assert_eq!(out, [vec![], vec![Intent::SetFocus(Focus::Terminal)]]);
}

#[test]
fn a_press_in_a_tracked_pane_replaces_an_earlier_click_on_a_control() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.x + 3, 2);
    let out = events_with(
        &mut c,
        WIDE,
        sgr_mouse(),
        &[
            mouse(DOWN, button),
            mouse(UP, button),
            mouse(DOWN, p),
            mouse(UP, button),
        ],
    );
    assert_eq!(out[1], [Intent::BeginNewSession]);
    assert_eq!(out[3], [], "the release on the button wasn't pressed there");
}

#[test]
fn a_press_on_nothing_replaces_an_earlier_press_on_a_control() {
    let mut c = sessions(1, WIDE);
    let buf = buffer(&c, WIDE);
    let button = at(&buf, "+ New session");
    let empty = below(at(&buf, "name-01"), 3);
    let out = events(
        &mut c,
        WIDE,
        &[mouse(DOWN, button), mouse(DOWN, empty), mouse(UP, button)],
    );
    assert!(out.iter().all(Vec::is_empty), "{out:?}");
}

// ---------- the close button and the footer ----------

/// Where `text` is drawn on row `y`, searching from the right.
fn last_on_row(buf: &Buffer, y: u16, text: &str) -> Position {
    (0..buf.area.width)
        .rev()
        .map(|x| Position::new(x, y))
        .find(|p| buf[*p].symbol() == text)
        .unwrap_or_else(|| panic!("{text:?} not drawn on row {y}: {buf:?}"))
}

#[test]
fn each_rows_close_button_is_the_last_cell_of_its_first_line() {
    let c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    for (name, id) in [("name-02", "s2"), ("name-01", "s1")] {
        let row = at(&buf, name);
        let x = last_on_row(&buf, row.y, "×");
        assert_eq!(
            buf[(x.x + 1, x.y)].symbol(),
            "│",
            "right against the border"
        );
        assert_eq!(
            hit_at(&c, WIDE, x),
            Some(Target::CloseSession(SessionId::new(id)))
        );
        assert_eq!(hit_at(&c, WIDE, Position::new(x.x - 1, x.y)), session(id));
        assert_eq!(hit_at(&c, WIDE, below(x, 1)), session(id));
    }
}

#[test]
fn the_footer_has_detach_and_help_buttons() {
    let c = sessions(1, WIDE);
    let buf = buffer(&c, WIDE);
    let detach = at(&buf, "Detach");
    assert_eq!(hit_at(&c, WIDE, detach), Some(Target::Detach));
    assert_eq!(hit_at(&c, WIDE, right(detach, 5)), Some(Target::Detach));
    let help = last_on_row(&buf, detach.y, "?");
    assert!(help.x > detach.x + 5);
    assert_eq!(hit_at(&c, WIDE, help), Some(Target::Help));
    assert_eq!(
        hit_at(&c, WIDE, Position::new(detach.x - 1, detach.y)),
        None
    );
}

// ---------- the context menu ----------

const RIGHT_DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Right);
const RIGHT_UP: MouseEventKind = MouseEventKind::Up(MouseButton::Right);

fn right_click(c: &mut ClientDriver, window: Rect, p: Position) -> Vec<Vec<Intent>> {
    events(c, window, &[mouse(RIGHT_DOWN, p), mouse(RIGHT_UP, p)])
}

fn cell(p: Position) -> Cell {
    Cell { col: p.x, row: p.y }
}

#[test]
fn right_clicking_a_row_opens_its_menu_at_the_click() {
    let mut c = sessions(2, WIDE);
    let second = below(at(&buffer(&c, WIDE), "name-01"), 1);
    let out = right_click(&mut c, WIDE, second);
    let open = Intent::OpenMenu {
        session: SessionId::new("s1"),
        at: cell(second),
    };
    assert_eq!(out, [vec![], vec![open]]);
    let buf = buffer(&c, WIDE);
    let rename = at(&buf, "Rename");
    let close = at(&buf, "Close");
    assert_eq!(close, below(rename, 1));
    // The menu's top-left corner is the click; the items are inside its border.
    assert_eq!(buf[second].symbol(), "┌", "{buf:?}");
    assert_eq!(rename.y, second.y + 1);
    assert_eq!(c.client().selected(), Some(&SessionId::new("s2")));
}

#[test]
fn a_right_press_and_release_on_different_rows_opens_nothing() {
    let mut c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(RIGHT_DOWN, at(&buf, "name-02")),
            mouse(RIGHT_UP, at(&buf, "name-01")),
        ],
    );
    assert!(out.iter().all(Vec::is_empty), "{out:?}");
}

#[test]
fn the_menu_is_kept_inside_the_window() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::OpenMenu {
        session: SessionId::new("s1"),
        at: Cell {
            col: WIDE.right() - 1,
            row: WIDE.bottom() - 1,
        },
    });
    let buf = buffer(&c, WIDE);
    let close = at(&buf, "Close");
    assert_eq!(
        buf[(WIDE.right() - 1, WIDE.bottom() - 1)].symbol(),
        "┘",
        "{buf:?}"
    );
    assert_eq!(
        hit_at(&c, WIDE, close),
        Some(Target::MenuItem(MenuItem::Close))
    );
}

#[test]
fn the_menus_items_are_hit_across_their_rows() {
    let mut c = sessions(1, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    let buf = buffer(&c, WIDE);
    let rename = at(&buf, "Rename");
    assert_eq!(
        hit_at(&c, WIDE, rename),
        Some(Target::MenuItem(MenuItem::Rename))
    );
    assert_eq!(
        hit_at(&c, WIDE, Position::new(rename.x - 1, rename.y)),
        Some(Target::MenuItem(MenuItem::Rename))
    );
    assert_eq!(
        hit_at(&c, WIDE, below(rename, 1)),
        Some(Target::MenuItem(MenuItem::Close))
    );
    // Its border is the menu, but no item.
    assert_eq!(hit_at(&c, WIDE, row), Some(Target::Popup));
    assert_eq!(hit_at(&c, WIDE, below(rename, 3)), Some(Target::Backdrop));
}

#[test]
fn clicking_rename_in_the_menu_opens_the_rename_popup_for_that_session() {
    let mut c = sessions(2, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    let rename = at(&buffer(&c, WIDE), "Rename");
    assert_eq!(
        click(&mut c, WIDE, rename),
        [vec![], vec![Intent::MenuChoose(MenuItem::Rename)]]
    );
    assert!(
        matches!(c.client().popup(), Some(Popup::Rename { session, .. }) if session.as_str() == "s1"),
        "{:?}",
        c.client().popup()
    );
}

#[test]
fn clicking_close_in_the_menu_asks_to_close_that_session() {
    let mut c = sessions(2, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    let close = at(&buffer(&c, WIDE), "Close");
    assert_eq!(
        click(&mut c, WIDE, close),
        [vec![], vec![Intent::MenuChoose(MenuItem::Close)]]
    );
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose {
            session: SessionId::new("s1")
        })
    );
}

#[test]
fn clicking_outside_the_menu_dismisses_it() {
    let mut c = sessions(2, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    let pane = layout(WIDE).pane;
    let out = click(&mut c, WIDE, Position::new(pane.x + 30, 20));
    assert_eq!(out, [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn right_clicking_outside_the_menu_dismisses_it() {
    let mut c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    right_click(&mut c, WIDE, at(&buf, "name-01"));
    let out = right_click(&mut c, WIDE, at(&buf, "+ New session"));
    assert_eq!(out, [vec![], vec![Intent::Dismiss]]);
}

#[test]
fn a_press_on_a_menu_item_released_elsewhere_does_nothing() {
    let mut c = sessions(1, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    let buf = buffer(&c, WIDE);
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(DOWN, at(&buf, "Rename")),
            mouse(UP, at(&buf, "Close")),
        ],
    );
    assert!(out.iter().all(Vec::is_empty), "{out:?}");
}

// ---------- the collapsed sidebar ----------

/// Where `text` is drawn in the sidebar of `window`, from the bottom up.
fn in_sidebar(buf: &Buffer, window: Rect, text: &str) -> Position {
    let side = layout(window).sidebar;
    side.positions()
        .filter(|p| buf[*p].symbol() == text)
        .last()
        .unwrap_or_else(|| panic!("{text:?} not drawn in the sidebar: {buf:?}"))
}

#[test]
fn the_collapsed_sidebar_has_detach_and_help_buttons_at_the_bottom() {
    let c = sessions(1, NARROW);
    let buf = buffer(&c, NARROW);
    let detach = in_sidebar(&buf, NARROW, "⏏");
    let help = in_sidebar(&buf, NARROW, "?");
    assert_eq!(help.y, NARROW.bottom() - 1, "help is on the last line");
    assert_eq!(help, below(detach, 1), "detach is just above it");
    assert_eq!(hit_at(&c, NARROW, detach), Some(Target::Detach));
    assert_eq!(hit_at(&c, NARROW, help), Some(Target::Help));
    let left = Position::new(detach.x - 1, detach.y);
    assert_eq!(hit_at(&c, NARROW, left), None);
}

#[test]
fn clicking_the_collapsed_detach_quits_and_help_shows_it() {
    let mut c = sessions(1, NARROW);
    let buf = buffer(&c, NARROW);
    let detach = in_sidebar(&buf, NARROW, "⏏");
    let help = in_sidebar(&buf, NARROW, "?");
    assert_eq!(click(&mut c, NARROW, detach), [vec![], vec![Intent::Quit]]);
    assert_eq!(
        click(&mut c, NARROW, help),
        [vec![], vec![Intent::ShowHelp]]
    );
    assert_eq!(c.client().popup(), Some(&Popup::Help));
}

#[test]
fn the_collapsed_sidebars_buttons_stay_below_a_full_list() {
    let c = sessions(10, NARROW);
    let buf = buffer(&c, NARROW);
    let detach = in_sidebar(&buf, NARROW, "⏏");
    assert_eq!(hit_at(&c, NARROW, detach), Some(Target::Detach));
    // The last row shown ends above the buttons.
    let above = Position::new(detach.x, detach.y - 1);
    assert_ne!(buf[above].symbol(), "◌", "{buf:?}");
}

#[test]
fn a_collapsed_row_is_closed_by_right_click_then_close() {
    let mut c = sessions(2, NARROW);
    let buf = buffer(&c, NARROW);
    let plus = at(&buf, "+");
    // The second glyph down is the older session, s1.
    let glyph = (plus.y + 1..NARROW.bottom())
        .map(|y| Position::new(plus.x, y))
        .filter(|p| buf[*p].symbol() == "◌")
        .nth(1)
        .unwrap_or_else(|| panic!("two glyphs: {buf:?}"));
    right_click(&mut c, NARROW, glyph);
    let close = at(&buffer(&c, NARROW), "Close");
    click(&mut c, NARROW, close);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose {
            session: SessionId::new("s1")
        })
    );
}

// ---------- dialogs ----------

#[test]
fn the_rename_dialog_answers_by_its_buttons() {
    for (label, intent) in [("[Save]", Intent::Confirm), ("[Cancel]", Intent::Dismiss)] {
        let mut c = sessions(1, WIDE);
        c.user(Intent::BeginRename);
        let button = at(&buffer(&c, WIDE), label);
        assert_eq!(
            click(&mut c, WIDE, button),
            [vec![], vec![intent]],
            "{label}"
        );
    }
}

#[test]
fn the_help_closes_by_its_button() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::ShowHelp);
    let button = at(&buffer(&c, WIDE), "[Close]");
    assert_eq!(click(&mut c, WIDE, button), [vec![], vec![Intent::Dismiss]]);
}

#[test]
fn clicking_inside_a_dialog_off_its_buttons_does_nothing() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::BeginClose);
    let text = at(&buffer(&c, WIDE), "Close name-01?");
    assert_eq!(hit_at(&c, WIDE, text), Some(Target::Popup));
    assert_eq!(click(&mut c, WIDE, text), [vec![], vec![]]);
    assert!(c.client().popup().is_some());
}

#[test]
fn a_popup_takes_clicks_over_a_session_that_tracks_the_mouse() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::BeginClose);
    let pane = layout(WIDE).pane;
    let p = Position::new(pane.right() - 2, 1);
    let out = events_with(&mut c, WIDE, sgr_mouse(), &[mouse(DOWN, p), mouse(UP, p)]);
    assert_eq!(out, [vec![], vec![Intent::Dismiss]]);
}

#[test]
fn the_wheel_does_nothing_under_a_popup() {
    let mut c = sessions(15, SHORT);
    let row = at(&buffer(&c, SHORT), "name-15");
    c.user(Intent::ShowHelp);
    let out = events(&mut c, SHORT, &[mouse(MouseEventKind::ScrollDown, row)]);
    assert_eq!(out, [vec![]]);
}

#[test]
fn a_press_and_release_of_different_buttons_on_one_row_do_nothing() {
    for (down, up) in [(DOWN, RIGHT_UP), (RIGHT_DOWN, UP)] {
        let mut c = sessions(2, WIDE);
        let row = at(&buffer(&c, WIDE), "name-01");
        let out = events(&mut c, WIDE, &[mouse(down, row), mouse(up, row)]);
        assert!(
            out.iter().all(Vec::is_empty),
            "{down:?} then {up:?}: {out:?}"
        );
    }
}

#[test]
fn a_long_name_stops_a_blank_column_short_of_the_close_button() {
    let mut c = sessions(1, WIDE);
    c.host().rename("s1", "a-name-much-longer-than-the-sidebar");
    c.sync();
    let buf = buffer(&c, WIDE);
    let start = at(&buf, "a-name-much");
    let x = last_on_row(&buf, start.y, "×");
    assert_eq!(buf[(x.x - 1, x.y)].symbol(), " ", "{buf:?}");
    assert_ne!(
        buf[(x.x - 2, x.y)].symbol(),
        " ",
        "the name fills up to it: {buf:?}"
    );
}

// ---------- the profile picker ----------

/// A connected client with session `s1` (named `name-01`), whose host has the `claude`, `codex` and `shell` profiles,
/// `codex` the default, told the size of `window`.
fn with_profiles(window: Rect) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("claude", AgentKind::ClaudeCode),
            ("codex", AgentKind::Codex),
            ("shell", AgentKind::Shell),
        ],
        "codex",
    );
    c.host().new_session("s1");
    c.host().rename("s1", "name-01");
    c.sync();
    c.user(Intent::SidebarRows(shepherd_tui::layout::sidebar_rows(
        window,
    )));
    c
}

/// The picker open over `with_profiles`, opened by clicking `+ New session`.
fn picker(window: Rect) -> ClientDriver {
    let mut c = with_profiles(window);
    let button = at(&buffer(&c, window), "+");
    click(&mut c, window, button);
    assert_eq!(c.client().popup(), Some(&Popup::PickProfile));
    c
}

fn profile(name: &str) -> Option<Target> {
    Some(Target::Profile(name.to_owned()))
}

#[test]
fn clicking_new_session_with_several_profiles_opens_the_picker() {
    for window in [WIDE, NARROW] {
        let c = picker(window);
        let buf = buffer(&c, window);
        at(&buf, "codex");
        at(&buf, "[Cancel]");
    }
}

#[test]
fn the_pickers_rows_are_the_profiles_default_first_and_marked() {
    let c = picker(WIDE);
    let buf = buffer(&c, WIDE);
    let first = at(&buf, "codex");
    assert_eq!(at(&buf, "claude"), below(first, 1));
    assert_eq!(at(&buf, "shell"), below(first, 2));
    let mark = at(&buf, "default");
    assert_eq!(mark.y, first.y, "the default's row is marked: {buf:?}");
}

#[test]
fn each_pickers_row_is_hit_across_the_dialog() {
    let c = picker(WIDE);
    let buf = buffer(&c, WIDE);
    let claude = at(&buf, "claude");
    assert_eq!(hit_at(&c, WIDE, claude), profile("claude"));
    assert_eq!(hit_at(&c, WIDE, right(claude, 20)), profile("claude"));
    assert_eq!(hit_at(&c, WIDE, below(claude, 1)), profile("shell"));
    assert_eq!(
        hit_at(&c, WIDE, Position::new(claude.x - 2, claude.y)),
        Some(Target::Popup),
        "the border"
    );
}

#[test]
fn the_picker_cancels_by_its_button() {
    let mut c = picker(WIDE);
    let cancel = at(&buffer(&c, WIDE), "[Cancel]");
    assert_eq!(click(&mut c, WIDE, cancel), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn clicking_outside_the_picker_dismisses_it() {
    let mut c = picker(WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    assert_eq!(hit_at(&c, WIDE, row), Some(Target::Backdrop));
    assert_eq!(click(&mut c, WIDE, row), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

/// A connected client whose host has `n` shell profiles, `p001` (the default) to `p<n>`, with the picker open by
/// clicking `+ New session`.
fn picker_of(n: usize, window: Rect) -> ClientDriver {
    let names: Vec<String> = (1..=n).map(|i| format!("p{i:03}")).collect();
    let profiles: Vec<(&str, AgentKind)> = names
        .iter()
        .map(|n| (n.as_str(), AgentKind::Shell))
        .collect();
    let mut c = ClientDriver::new();
    c.connect_with_profiles(&profiles, "p001");
    c.user(Intent::SidebarRows(shepherd_tui::layout::sidebar_rows(
        window,
    )));
    let button = at(&buffer(&c, window), "+");
    click(&mut c, window, button);
    assert_eq!(c.client().popup(), Some(&Popup::PickProfile));
    c
}

#[test]
fn a_long_profile_list_fits_the_window_and_shows_its_first_rows() {
    let c = picker_of(30, SHORT);
    let buf = buffer(&c, SHORT);
    // 12 rows: the border, the prompt, the buttons and the border leave 8 for profiles.
    let first = at(&buf, "p001");
    assert_eq!(at(&buf, "p008"), below(first, 7));
    assert_eq!(find(&buf, "p009"), None, "{buf:?}");
    at(&buf, "[Cancel]");
}

#[test]
fn the_wheel_over_the_picker_scrolls_it() {
    let mut c = picker_of(30, SHORT);
    let buf = buffer(&c, SHORT);
    let (top, row) = (at(&buf, "p001"), at(&buf, "p003"));
    let prompt = at(&buf, "Which agent");
    assert_eq!(hit_at(&c, SHORT, prompt), Some(Target::Popup));
    assert_eq!(
        events(
            &mut c,
            SHORT,
            &[
                mouse(MouseEventKind::ScrollDown, row),
                mouse(MouseEventKind::ScrollDown, prompt),
                mouse(MouseEventKind::ScrollUp, row),
            ]
        ),
        [
            vec![Intent::ScrollPicker { by: 1, shown: 8 }],
            vec![Intent::ScrollPicker { by: 1, shown: 8 }],
            vec![Intent::ScrollPicker { by: -1, shown: 8 }],
        ]
    );
    let buf = buffer(&c, SHORT);
    assert_eq!(at(&buf, "p002"), top, "scrolled down one row: {buf:?}");
    assert_eq!(find(&buf, "p001"), None, "{buf:?}");
}

#[test]
fn a_profile_below_the_fold_is_reached_by_wheel_then_click() {
    let mut c = picker_of(30, SHORT);
    let over = at(&buffer(&c, SHORT), "p001");
    let wheel: Vec<Event> = (0..100)
        .map(|_| mouse(MouseEventKind::ScrollDown, over))
        .collect();
    events(&mut c, SHORT, &wheel);
    let buf = buffer(&c, SHORT);
    assert_eq!(find(&buf, "p001"), None, "{buf:?}");
    let last = at(&buf, "p030");
    assert_eq!(at(&buf, "p023"), Position::new(last.x, last.y - 7));
    assert_eq!(
        click(&mut c, SHORT, last),
        [vec![], vec![Intent::PickProfile("p030".to_owned())]]
    );
}

#[test]
fn the_wheel_outside_the_picker_does_not_scroll_it() {
    let mut c = picker_of(30, SHORT);
    let outside = Position::new(1, SHORT.bottom() - 1);
    assert_eq!(hit_at(&c, SHORT, outside), Some(Target::Backdrop));
    assert_eq!(
        events(&mut c, SHORT, &[mouse(MouseEventKind::ScrollDown, outside)]),
        [vec![]]
    );
}

#[test]
fn a_huge_profile_list_is_capped_to_the_window() {
    // More profiles than a u16 counts, on both sides of where the count wraps.
    for n in [65_535, 65_536] {
        let c = picker_of(n, SHORT);
        let buf = buffer(&c, SHORT);
        let first = at(&buf, "p001");
        assert_eq!(at(&buf, "p008"), below(first, 7), "{n}: {buf:?}");
        at(&buf, "[Cancel]");
    }
}

/// A connected client whose host has `profiles`, the first the default, with the picker open by clicking
/// `+ New session`.
fn picker_with(profiles: &[(&str, AgentKind)], window: Rect) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(profiles, profiles[0].0);
    let button = at(&buffer(&c, window), "+");
    click(&mut c, window, button);
    assert_eq!(c.client().popup(), Some(&Popup::PickProfile));
    c
}

#[test]
fn the_pickers_columns_are_as_wide_as_their_longest_entries() {
    let c = picker_with(&[("xq", AgentKind::Shell), ("xqz", AgentKind::Shell)], WIDE);
    let buf = buffer(&c, WIDE);
    let name = at(&buf, "xq ");
    // The name column is 3 wide and the agent column 5 (`shell`), each followed by 2 blanks.
    assert_eq!(at(&buf, "shell"), right(name, 5));
    assert_eq!(at(&buf, "(default)"), right(name, 12));
}

#[test]
fn a_long_profile_name_is_cut_short_so_the_default_mark_stays_visible() {
    let long = "a-profile-name-far-too-long-for-the-dialog";
    for window in [WIDE, NARROW] {
        let mut c = picker_with(
            &[(long, AgentKind::ClaudeCode), ("shell", AgentKind::Shell)],
            window,
        );
        let buf = buffer(&c, window);
        let start = at(&buf, "a-profile");
        let mark = at(&buf, "(default)");
        assert_eq!(mark.y, start.y, "{buf:?}");
        assert_eq!(find(&buf, long), None, "{buf:?}");
        let cut = buf[(at(&buf, "claude-code").x - 3, start.y)]
            .symbol()
            .to_owned();
        assert_eq!(cut, "…", "{buf:?}");
        assert_eq!(
            click(&mut c, window, start),
            [vec![], vec![Intent::PickProfile(long.to_owned())]]
        );
    }
}

// ---------- resume ----------

/// [`sessions`] plus a newest Claude Code session `s9`, `name-09`, whose agent reported `agent_session` and exited,
/// with its menu open.
fn exited_with_menu(agent_session: Option<&str>) -> ClientDriver {
    let mut c = sessions(1, WIDE);
    c.host()
        .new_session_with("s9", shepherd_testkit::builders::claude_session_spec());
    c.host().rename("s9", "name-09");
    c.host().agent_started("s9", "claude-code", agent_session);
    c.host().pty_exited("s9", Some(0));
    c.sync();
    let row = at(&buffer(&c, WIDE), "name-09");
    right_click(&mut c, WIDE, row);
    c
}

#[test]
fn clicking_resume_in_an_exited_sessions_menu_resumes_it() {
    let mut c = exited_with_menu(Some("abc123"));
    let buf = buffer(&c, WIDE);
    let resume = at(&buf, "Resume");
    assert_eq!(resume, below(at(&buf, "Close"), 1));
    assert_eq!(
        click(&mut c, WIDE, resume),
        [vec![], vec![Intent::MenuChoose(MenuItem::Resume)]]
    );
}

#[test]
fn an_unavailable_resume_is_shown_with_its_reason_and_clicks_do_nothing() {
    let mut c = exited_with_menu(None);
    let buf = buffer(&c, WIDE);
    let resume = at(&buf, "Resume (no agent session id)");
    assert_eq!(hit_at(&c, WIDE, resume), Some(Target::Popup));
    assert_eq!(click(&mut c, WIDE, resume), [vec![], vec![]]);
    assert!(matches!(c.client().popup(), Some(Popup::Menu { .. })));
}

#[test]
fn a_running_sessions_menu_has_no_resume() {
    let mut c = sessions(1, WIDE);
    let row = at(&buffer(&c, WIDE), "name-01");
    right_click(&mut c, WIDE, row);
    assert_eq!(find(&buffer(&c, WIDE), "Resume"), None);
}

// ---------- the workspace ----------

/// `sessions(1, window)` on a host that has used `/src/api`, then `/src/shepherd` (the current workspace).
fn with_workspaces(window: Rect) -> ClientDriver {
    let mut c = sessions(1, window);
    c.host().select_workspace("/src/api");
    c.host().select_workspace("/src/shepherd");
    c.sync();
    c
}

/// The switcher open over `with_workspaces`, opened by clicking the workspace in the sidebar.
fn switcher(window: Rect) -> ClientDriver {
    let mut c = with_workspaces(window);
    let label = if window == NARROW {
        "⌂"
    } else {
        "⌂ shepherd"
    };
    let button = at(&buffer(&c, window), label);
    assert_eq!(
        click(&mut c, window, button),
        [vec![], vec![Intent::BeginWorkspace]]
    );
    c
}

fn workspace_row(path: &str) -> Option<Target> {
    Some(Target::WorkspaceRow(path.into()))
}

#[test]
fn the_workspace_is_shown_above_new_session_and_hit_across_its_label() {
    let c = with_workspaces(WIDE);
    let buf = buffer(&c, WIDE);
    let label = "⌂ shepherd";
    let start = at(&buf, label);
    assert_eq!(at(&buf, "+ New session"), below(start, 1));
    let end = right(start, label.chars().count() as u16 - 1);
    assert_eq!(hit_at(&c, WIDE, start), Some(Target::Workspace));
    assert_eq!(hit_at(&c, WIDE, end), Some(Target::Workspace));
    assert_eq!(hit_at(&c, WIDE, right(end, 1)), None);
}

#[test]
fn without_a_workspace_the_sidebar_offers_to_choose_one() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "⌂ choose a workspace");
    assert_eq!(
        click(&mut c, WIDE, button),
        [vec![], vec![Intent::BeginWorkspace]]
    );
}

#[test]
fn clicking_the_workspace_opens_the_switcher_in_both_sidebars() {
    for window in [WIDE, NARROW] {
        let c = switcher(window);
        assert!(
            matches!(c.client().popup(), Some(Popup::Workspace { .. })),
            "{window:?}"
        );
        let buf = buffer(&c, window);
        at(&buf, "[Open]");
        at(&buf, "[Cancel]");
    }
}

#[test]
fn the_switcher_lists_the_workspaces_used_before_current_first() {
    let c = switcher(WIDE);
    let buf = buffer(&c, WIDE);
    let current = at(&buf, "/src/shepherd");
    assert_eq!(at(&buf, "/src/api"), below(current, 1));
    let mark = at(&buf, "(current)");
    assert_eq!(mark.y, current.y, "{buf:?}");
    assert_eq!(hit_at(&c, WIDE, current), workspace_row("/src/shepherd"));
    assert_eq!(
        hit_at(&c, WIDE, right(current, 25)),
        workspace_row("/src/shepherd")
    );
    assert_eq!(
        hit_at(&c, WIDE, below(current, 1)),
        workspace_row("/src/api")
    );
}

#[test]
fn clicking_a_workspace_used_before_asks_to_select_it() {
    let mut c = switcher(WIDE);
    let row = at(&buffer(&c, WIDE), "/src/api");
    assert_eq!(
        click(&mut c, WIDE, row),
        [vec![], vec![Intent::PickWorkspace("/src/api".into())]]
    );
}

#[test]
fn the_switcher_opens_a_typed_path_by_its_button_and_cancels_by_the_other() {
    let mut c = switcher(WIDE);
    c.user(Intent::EditWorkspacePath("/src/new".into()));
    let buf = buffer(&c, WIDE);
    at(&buf, "/src/new");
    let open = at(&buf, "[Open]");
    assert_eq!(click(&mut c, WIDE, open), [vec![], vec![Intent::Confirm]]);
    let cancel = at(&buffer(&c, WIDE), "[Cancel]");
    assert_eq!(click(&mut c, WIDE, cancel), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn a_refused_path_is_shown_in_the_switcher() {
    let mut c = switcher(WIDE);
    c.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(shepherd_core::wire::WireError::InvalidParams(
            "/tmp/x is not a repo".into(),
        )),
    );
    at(&buffer(&c, WIDE), "/tmp/x is not a repo");
}

/// `sessions(1, window)` on a host that has used `paths` in order, the last the current workspace, with the switcher
/// open by clicking the workspace in the sidebar.
fn switcher_with(paths: &[String], window: Rect) -> ClientDriver {
    let mut c = sessions(1, window);
    for path in paths {
        c.host().select_workspace(path);
    }
    c.sync();
    let button = at(&buffer(&c, window), "⌂");
    assert_eq!(
        click(&mut c, window, button),
        [vec![], vec![Intent::BeginWorkspace]]
    );
    c
}

/// The switcher over `n` workspaces used before, `/src/w001` (the oldest) to `/src/w<n>` (current), so the list
/// starts at `/src/w<n>` and ends at `/src/w001`.
fn switcher_of(n: usize, window: Rect) -> ClientDriver {
    let paths: Vec<String> = (1..=n).map(|i| format!("/src/w{i:03}")).collect();
    switcher_with(&paths, window)
}

#[test]
fn a_long_workspace_list_fits_the_window_and_shows_its_first_rows() {
    let c = switcher_of(30, SHORT);
    let buf = buffer(&c, SHORT);
    // 12 rows: the border, `Used before:`, the prompt, the input, the hint, the buttons and the border leave 5.
    let first = at(&buf, "/src/w030");
    assert_eq!(at(&buf, "/src/w026"), below(first, 4));
    assert_eq!(find(&buf, "/src/w025"), None, "{buf:?}");
    at(&buf, "Used before:");
    at(&buf, "Open a repo by path:");
    at(&buf, "[Open]");
}

#[test]
fn the_wheel_over_the_switcher_scrolls_it() {
    let mut c = switcher_of(30, SHORT);
    let buf = buffer(&c, SHORT);
    let (top, row) = (at(&buf, "/src/w030"), at(&buf, "/src/w028"));
    let prompt = at(&buf, "Open a repo");
    let open = at(&buf, "[Open]");
    assert_eq!(hit_at(&c, SHORT, prompt), Some(Target::Popup));
    let down = |p| mouse(MouseEventKind::ScrollDown, p);
    assert_eq!(
        events(
            &mut c,
            SHORT,
            &[
                down(row),
                down(prompt),
                down(open),
                mouse(MouseEventKind::ScrollUp, row),
            ]
        ),
        [
            vec![Intent::ScrollSwitcher { by: 1, shown: 5 }],
            vec![Intent::ScrollSwitcher { by: 1, shown: 5 }],
            vec![Intent::ScrollSwitcher { by: 1, shown: 5 }],
            vec![Intent::ScrollSwitcher { by: -1, shown: 5 }],
        ]
    );
    let buf = buffer(&c, SHORT);
    assert_eq!(
        at(&buf, "/src/w028"),
        top,
        "scrolled down two rows: {buf:?}"
    );
    assert_eq!(find(&buf, "/src/w029"), None, "{buf:?}");
}

#[test]
fn a_workspace_below_the_fold_is_reached_by_wheel_then_click() {
    let mut c = switcher_of(30, SHORT);
    let over = at(&buffer(&c, SHORT), "/src/w030");
    let wheel: Vec<Event> = (0..100)
        .map(|_| mouse(MouseEventKind::ScrollDown, over))
        .collect();
    events(&mut c, SHORT, &wheel);
    let buf = buffer(&c, SHORT);
    assert_eq!(find(&buf, "/src/w030"), None, "{buf:?}");
    let last = at(&buf, "/src/w001");
    assert_eq!(at(&buf, "/src/w005"), Position::new(last.x, last.y - 4));
    assert_eq!(
        click(&mut c, SHORT, last),
        [vec![], vec![Intent::PickWorkspace("/src/w001".into())]]
    );
}

#[test]
fn the_wheel_outside_the_switcher_does_not_scroll_it() {
    let mut c = switcher_of(30, SHORT);
    let outside = Position::new(1, SHORT.bottom() - 1);
    assert_eq!(hit_at(&c, SHORT, outside), Some(Target::Backdrop));
    assert_eq!(
        events(&mut c, SHORT, &[mouse(MouseEventKind::ScrollDown, outside)]),
        [vec![]]
    );
}

#[test]
fn a_huge_workspace_list_is_capped_to_the_window() {
    // Far more than any window shows. Not enough to wrap a u16 count, as the picker's test does: the host folds a
    // select in time linear in the workspaces, so tens of thousands take minutes.
    let n = 2_000;
    let c = switcher_of(n, SHORT);
    let buf = buffer(&c, SHORT);
    let first = at(&buf, &format!("/src/w{n:03}"));
    assert_eq!(
        at(&buf, &format!("/src/w{:03}", n - 4)),
        below(first, 4),
        "{buf:?}"
    );
    at(&buf, "[Open]");
}

#[test]
fn a_long_path_is_cut_from_the_left_so_the_repo_name_and_mark_stay_visible() {
    let long = "/Users/someone/a/deeply/nested/directory/full/of/projects/shepherd";
    for window in [WIDE, NARROW] {
        let mut c = switcher_with(&["/src/api".to_owned(), long.to_owned()], window);
        let buf = buffer(&c, window);
        assert_eq!(find(&buf, long), None, "{buf:?}");
        let end = at(&buf, "/shepherd  (current)");
        let start = Position::new(at(&buf, "/src/api").x, end.y);
        assert_eq!(buf[start].symbol(), "…", "{buf:?}");
        assert_eq!(
            click(&mut c, window, start),
            [vec![], vec![Intent::PickWorkspace(long.into())]]
        );
    }
}

#[test]
fn sessions_from_another_workspace_are_dimmed_and_say_which() {
    let mut c = sessions(1, WIDE);
    c.host().select_workspace("/src/api");
    c.host().new_session("s2");
    c.host().rename("s2", "name-02");
    // Running in its worktree, so line 2 isn't `creating worktree…`.
    c.host().worktree_ready("s2");
    c.host().select_workspace("/src/shepherd");
    c.sync();
    let buf = buffer(&c, WIDE);
    let elsewhere = at(&buf, "name-02");
    let here = at(&buf, "name-01");
    use ratatui::style::Modifier;
    assert!(buf[elsewhere].modifier.contains(Modifier::DIM), "{buf:?}");
    assert!(!buf[here].modifier.contains(Modifier::DIM), "{buf:?}");
    assert_eq!(at(&buf, "in api").y, elsewhere.y + 1, "{buf:?}");
}
