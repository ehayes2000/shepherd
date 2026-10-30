//! Clicking and scrolling: `hit` finds the control under a cell, and `map_event` turns presses, releases and the wheel
//! into intents. Every test clicks where the control is drawn, found in the rendered buffer.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::{Focus, Intent};
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
    assert_eq!(
        hit_at(&c, WIDE, Position::new(border - 1, row.y)),
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
fn other_buttons_on_a_row_do_nothing() {
    let mut c = sessions(2, WIDE);
    let second = at(&buffer(&c, WIDE), "name-01");
    let out = events(
        &mut c,
        WIDE,
        &[
            mouse(MouseEventKind::Down(MouseButton::Right), second),
            mouse(MouseEventKind::Up(MouseButton::Right), second),
        ],
    );
    assert_eq!(out, [vec![], vec![]]);
}

#[test]
fn clicks_under_a_popup_do_nothing() {
    let mut c = sessions(1, WIDE);
    let button = at(&buffer(&c, WIDE), "+ New session");
    c.user(Intent::ShowHelp);
    assert_eq!(click(&mut c, WIDE, button), [vec![], vec![]]);
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
