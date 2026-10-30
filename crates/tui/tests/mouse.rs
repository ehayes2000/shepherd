//! Clicking and scrolling: `hit` finds the control under a cell, and `map_event` turns presses, releases and the wheel
//! into intents. Every test clicks where the control is drawn, found in the rendered buffer.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::{Cell, Focus, Intent, MenuItem, Popup};
use shepherd_core::ids::SessionId;
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
    let last = at(&buf, "name-08");
    assert_eq!(hit_at(&c, SHORT, below(last, 1)), session("s8"));
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
fn clicking_new_session_creates_one_on_release() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    assert_eq!(
        click(&mut c, WIDE, button),
        [vec![], vec![Intent::NewSession]]
    );
}

#[test]
fn clicking_new_session_in_the_collapsed_sidebar_creates_one() {
    let mut c = sessions(1, NARROW);
    let plus = at(&buffer(&c, NARROW), "+");
    assert_eq!(
        click(&mut c, NARROW, plus),
        [vec![], vec![Intent::NewSession]]
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
fn clicking_a_row_selects_it_and_focuses_the_terminal() {
    let mut c = sessions(2, WIDE);
    c.user(Intent::SetFocus(Focus::Command));
    let second = at(&buffer(&c, WIDE), "name-01");
    let out = click(&mut c, WIDE, below(second, 1));
    assert_eq!(
        out[1],
        [
            Intent::SelectSession(SessionId::new("s1")),
            Intent::SetFocus(Focus::Terminal)
        ]
    );
    assert_eq!(c.client().selected(), Some(&SessionId::new("s1")));
    assert_eq!(c.client().focus(), Focus::Terminal);
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
            vec![Intent::Input(b"\x1b[<0;4;3M".to_vec())],
            vec![Intent::Input(b"\x1b[<0;4;3m".to_vec())]
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
    assert_eq!(out[1], [Intent::NewSession]);
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
fn clicking_a_rows_close_button_asks_to_close_it_without_selecting_it() {
    let mut c = sessions(2, WIDE);
    let buf = buffer(&c, WIDE);
    let x = last_on_row(&buf, at(&buf, "name-01").y, "×");
    let out = click(&mut c, WIDE, x);
    assert_eq!(
        out,
        [
            vec![],
            vec![Intent::BeginCloseSession(SessionId::new("s1"))]
        ]
    );
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose {
            session: SessionId::new("s1")
        })
    );
    assert_eq!(c.client().selected(), Some(&SessionId::new("s2")));
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

#[test]
fn clicking_detach_quits_and_clicking_help_shows_it() {
    let mut c = sessions(1, WIDE);
    let buf = buffer(&c, WIDE);
    let detach = at(&buf, "Detach");
    let help = last_on_row(&buf, detach.y, "?");
    assert_eq!(click(&mut c, WIDE, detach), [vec![], vec![Intent::Quit]]);
    assert_eq!(click(&mut c, WIDE, help), [vec![], vec![Intent::ShowHelp]]);
    assert_eq!(c.client().popup(), Some(&Popup::Help));
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

// ---------- dialogs ----------

#[test]
fn the_close_confirmation_answers_by_its_buttons() {
    for (label, intent) in [("[Close]", Intent::Confirm), ("[Cancel]", Intent::Dismiss)] {
        let mut c = sessions(1, WIDE);
        c.user(Intent::BeginClose);
        let button = at(&buffer(&c, WIDE), label);
        assert_eq!(
            click(&mut c, WIDE, right(button, 2)),
            [vec![], vec![intent]],
            "{label}"
        );
        assert_eq!(c.client().popup(), None, "{label}");
    }
}

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
fn clicking_outside_a_dialog_cancels_it() {
    let mut c = sessions(1, WIDE);
    c.user(Intent::BeginRename);
    let row = at(&buffer(&c, WIDE), "name-01");
    assert_eq!(hit_at(&c, WIDE, row), Some(Target::Backdrop));
    assert_eq!(click(&mut c, WIDE, row), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
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
