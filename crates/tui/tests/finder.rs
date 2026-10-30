//! The finder (T26): the sidebar's search field, the finder popup over sessions and orphan worktrees, and the offer
//! to start a session in a worktree. Drawn, clicked where drawn, and typed into.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::json;
use shepherd_core::client::{FinderItem, Intent, Orphan, Popup};
use shepherd_core::ids::SessionId;
use shepherd_term::Modes;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, map_key, render};

const WIDE: Rect = Rect::new(0, 0, 80, 24);
const NARROW: Rect = Rect::new(0, 0, 40, 12);

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

fn mouse(kind: MouseEventKind, p: Position) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: p.x,
        row: p.y,
        modifiers: KeyModifiers::NONE,
    })
}

/// What each event means, applied to the client as it comes.
fn events(c: &mut ClientDriver, window: Rect, events: &[Event]) -> Vec<Vec<Intent>> {
    let mut pointer = Pointer::default();
    events
        .iter()
        .map(|event| {
            let intents = map_event(event, c.client(), &Modes::default(), window, &mut pointer);
            for intent in &intents {
                c.user(intent.clone());
            }
            intents
        })
        .collect()
}

fn click(c: &mut ClientDriver, window: Rect, p: Position) -> Vec<Vec<Intent>> {
    let (down, up) = (
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    );
    events(c, window, &[mouse(down, p), mouse(up, p)])
}

fn key(c: &mut ClientDriver, code: KeyCode) -> Vec<Intent> {
    let intents = map_key(
        &KeyEvent::new(code, KeyModifiers::NONE),
        c.client(),
        &Modes::default(),
    );
    for intent in &intents {
        c.user(intent.clone());
    }
    intents
}

/// Three sessions in workspace `/src/shepherd`, each on the branch of its worktree: `fix-login` working, `docs` idle,
/// `refactor` (newest) waiting for its worktree.
fn sessions() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/shepherd");
    for (id, name) in [("s1", "fix-login"), ("s2", "docs"), ("s3", "refactor")] {
        c.host().new_session(id);
        c.host().rename(id, name);
    }
    c.host().worktree_ready("s1");
    c.host().worktree_ready("s2");
    c.host().append(
        Some("s2"),
        shepherd_core::acpp::Source::new(shepherd_core::acpp::SourceKind::Hook, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    c.sync();
    c
}

/// `sessions`, with the finder opened by clicking the search field and the host's answer listing two orphan worktrees.
fn finder(window: Rect) -> ClientDriver {
    let mut c = sessions();
    let label = if window == NARROW {
        "⌕"
    } else {
        "⌕ Search"
    };
    let field = at(&buffer(&c, window), label);
    assert_eq!(
        click(&mut c, window, field),
        [vec![], vec![Intent::BeginFind]]
    );
    answer_worktrees(&mut c);
    c
}

/// Answers the finder's worktree list with `old-idea` and a detached worktree.
fn answer_worktrees(c: &mut ClientDriver) {
    let asked = c.user(Intent::BeginFind);
    c.answer(
        &asked,
        Ok(json!({ "worktrees": [
            { "path": "/wt/shepherd/old-idea", "branch": "old-idea" },
            { "path": "/wt/shepherd/spike-7" },
        ] })),
    );
}

fn orphan() -> Orphan {
    Orphan {
        path: "/wt/shepherd/old-idea".into(),
        branch: Some("old-idea".into()),
    }
}

// ---------- snapshots ----------

#[test]
fn finder_with_an_empty_query() {
    let c = finder(WIDE);
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn finder_filtered() {
    let mut c = finder(WIDE);
    c.user(Intent::EditFind("o".into()));
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn finder_with_no_results() {
    let mut c = finder(WIDE);
    c.user(Intent::EditFind("zzz".into()));
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn finder_loading_the_worktrees() {
    let mut c = sessions();
    c.user(Intent::BeginFind);
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn offer_to_start_a_session_in_a_worktree() {
    let mut c = finder(WIDE);
    c.user(Intent::FinderChoose(FinderItem::Worktree(orphan())));
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn the_finder_keeps_its_top_while_typing_changes_its_height() {
    let mut c = finder(WIDE);
    let top = at(&buffer(&c, WIDE), "┌ find");
    c.user(Intent::EditFind("zzz".into()));
    assert_eq!(at(&buffer(&c, WIDE), "┌ find"), top);
}

// ---------- clicks ----------

#[test]
fn the_search_field_is_at_the_top_of_both_sidebars() {
    for window in [WIDE, NARROW] {
        let c = finder(window);
        assert!(
            matches!(c.client().popup(), Some(Popup::Finder { .. })),
            "{window:?}"
        );
    }
    let c = sessions();
    let buf = buffer(&c, WIDE);
    let field = at(&buf, "⌕ Search");
    assert_eq!(field.y, 0);
    assert_eq!(at(&buf, "⌂ shepherd").y, 1);
    assert_eq!(at(&buf, "+ New session").y, 2);
    // The whole width of the field is the field.
    let end = Position::new(layout(WIDE).sidebar.right() - 3, 0);
    assert_eq!(
        hit(&layout(WIDE), c.client(), end.x, end.y),
        Some(Target::Search)
    );
}

#[test]
fn clicking_a_session_in_the_finder_selects_it() {
    let mut c = finder(WIDE);
    let buf = buffer(&c, WIDE);
    // `fix-login` is in the sidebar too.
    let row = in_finder(&buf, "fix-login");
    let s1 = FinderItem::Session(SessionId::new("s1"));
    assert_eq!(
        click(&mut c, WIDE, row),
        [vec![], vec![Intent::FinderChoose(s1)]]
    );
    assert_eq!(c.client().selected(), Some(&SessionId::new("s1")));
    assert_eq!(c.client().popup(), None);
}

/// Where `text` starts inside the finder's box, below and right of its top-left corner.
fn in_finder(buf: &Buffer, text: &str) -> Position {
    let corner = at(buf, "┌ find");
    buf.area
        .positions()
        .filter(|p| p.x > corner.x && p.y > corner.y)
        .find(|p| find_at(buf, *p, text))
        .unwrap_or_else(|| panic!("{text:?} not in the finder: {buf:?}"))
}

fn find_at(buf: &Buffer, p: Position, text: &str) -> bool {
    text.chars().enumerate().all(|(i, ch)| {
        let x = p.x + i as u16;
        x < buf.area.right() && buf[(x, p.y)].symbol() == ch.to_string()
    })
}

#[test]
fn clicking_an_orphan_worktree_then_start_session_starts_one_in_it() {
    let mut c = finder(WIDE);
    let row = at(&buffer(&c, WIDE), "old-idea");
    assert_eq!(
        click(&mut c, WIDE, row),
        [
            vec![],
            vec![Intent::FinderChoose(FinderItem::Worktree(orphan()))]
        ]
    );
    let start = at(&buffer(&c, WIDE), "[Start session]");
    assert_eq!(click(&mut c, WIDE, start), [vec![], vec![Intent::Confirm]]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn clicking_outside_the_finder_closes_it() {
    let mut c = finder(WIDE);
    let corner = Position::new(WIDE.right() - 1, WIDE.bottom() - 1);
    assert_eq!(click(&mut c, WIDE, corner), [vec![], vec![Intent::Dismiss]]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn the_wheel_over_the_finder_scrolls_its_results() {
    let ids: Vec<String> = (1..=30).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    c.user(Intent::BeginFind);
    let corner = at(&buffer(&c, WIDE), "┌ find");
    let over = Position::new(corner.x + 6, corner.y + 3);
    events(&mut c, WIDE, &[mouse(MouseEventKind::ScrollDown, over)]);
    assert_eq!(c.client().finder_scroll(), 1);
}

// ---------- keys ----------

#[test]
fn typing_in_the_finder_filters_and_enter_opens_the_first_result() {
    let mut c = finder(WIDE);
    for ch in "doc".chars() {
        key(&mut c, KeyCode::Char(ch));
    }
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Finder {
            query: "doc".into()
        })
    );
    key(&mut c, KeyCode::Backspace);
    assert_eq!(key(&mut c, KeyCode::Enter), [Intent::Confirm]);
    assert_eq!(c.client().selected(), Some(&SessionId::new("s2")));
}

#[test]
fn esc_closes_the_finder() {
    let mut c = finder(WIDE);
    assert_eq!(key(&mut c, KeyCode::Esc), [Intent::Dismiss]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn pasting_into_the_finder_types_the_first_line() {
    let mut c = finder(WIDE);
    let intents = events(&mut c, WIDE, &[Event::Paste("old\nrest".into())]);
    assert_eq!(intents, [vec![Intent::EditFind("old".into())]]);
}

#[test]
fn the_offer_is_answered_by_enter_or_esc() {
    let mut c = finder(WIDE);
    c.user(Intent::FinderChoose(FinderItem::Worktree(orphan())));
    assert_eq!(key(&mut c, KeyCode::Esc), [Intent::Dismiss]);
    c.user(Intent::BeginFind);
    answer_worktrees(&mut c);
    c.user(Intent::FinderChoose(FinderItem::Worktree(orphan())));
    assert_eq!(key(&mut c, KeyCode::Enter), [Intent::Confirm]);
}

#[test]
fn a_session_row_shows_its_status_glyph() {
    // `docs` is idle: its glyph is drawn in the finder too.
    let c = finder(WIDE);
    let buf = buffer(&c, WIDE);
    let row = in_finder(&buf, "docs");
    assert_eq!(buf[(row.x - 2, row.y)].symbol(), "○");
}
