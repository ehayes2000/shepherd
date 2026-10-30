//! The cleanup view (T25b): the sidebar's `Cleanup` button, the view with its status columns, and its buttons. Drawn,
//! and clicked where drawn.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::{Value, json};
use shepherd_core::client::{ClientCommand, ClientEvent, Intent, Popup};
use shepherd_core::wire::{ClientRequest, WorktreeRemove};
use shepherd_term::Modes;
use shepherd_testkit::builders::{ClientDriver, requests, ts};
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, map_key, render};

const WIDE: Rect = Rect::new(0, 0, 80, 24);
const SHORT: Rect = Rect::new(0, 0, 80, 10);

fn buffer(c: &ClientDriver, window: Rect) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(window.width, window.height)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(c.client(), &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

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

/// What each event means, applied to the client as it comes, with every command the client returned.
fn events(
    c: &mut ClientDriver,
    window: Rect,
    events: &[Event],
) -> (Vec<Intent>, Vec<ClientCommand>) {
    let mut pointer = Pointer::default();
    let (mut intents, mut commands) = (Vec::new(), Vec::new());
    for event in events {
        for intent in map_event(event, c.client(), &Modes::default(), window, &mut pointer) {
            commands.extend(c.user(intent.clone()));
            intents.push(intent);
        }
    }
    (intents, commands)
}

fn click(c: &mut ClientDriver, window: Rect, p: Position) -> (Vec<Intent>, Vec<ClientCommand>) {
    let (down, up) = (
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    );
    events(c, window, &[mouse(down, p), mouse(up, p)])
}

fn status(dirty: bool, ahead: u32, merged: bool) -> Value {
    json!({ "dirty": dirty, "ahead": ahead, "pushed": false, "merged_into_default": merged })
}

/// A connected client in workspace `/src/shepherd` with a session `fix-login` that exited 28 days before the tick and
/// another, `docs`, still running.
fn in_workspace() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/shepherd");
    c.host().new_session("s1");
    c.host().rename("s1", "fix-login");
    c.host().worktree_ready("s1");
    c.host().pin_ts("2026-10-01T12:00:00Z");
    c.host().pty_exited("s1", Some(0));
    c.host().unpin_ts();
    c.host().new_session("s2");
    c.host().rename("s2", "docs");
    c.host().worktree_ready("s2");
    c.sync();
    c.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:00Z"),
    });
    c
}

/// [`in_workspace`], the view opened by clicking the footer's `Cleanup`, and the host's answers: three orphans and
/// `fix-login` merged.
fn opened() -> ClientDriver {
    let mut c = in_workspace();
    let button = at(&buffer(&c, WIDE), "Cleanup");
    let (intents, asked) = click(&mut c, WIDE, button);
    assert_eq!(intents, [Intent::BeginCleanup]);
    let checks = c.answer(
        &asked,
        Ok(json!({ "worktrees": [
            { "path": "/wt/shepherd/old-idea", "branch": "old-idea", "status": status(false, 0, true) },
            { "path": "/wt/shepherd/wip", "branch": "wip", "status": status(true, 2, false) },
            { "path": "/wt/shepherd/spike-7", "error": "not a git repository" },
        ], "cleanup_after_days": 7 })),
    );
    c.answer(&checks, Ok(json!({ "status": status(false, 0, true) })));
    c
}

// ---------- snapshots ----------

#[test]
fn the_footer_offers_cleanup_in_a_workspace() {
    let c = in_workspace();
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn cleanup_view_with_safe_and_unsafe_rows() {
    let c = opened();
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn cleanup_view_loading() {
    let mut c = in_workspace();
    c.user(Intent::BeginCleanup);
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

#[test]
fn cleanup_view_while_removing() {
    let mut c = opened();
    let first = c.user(Intent::CleanupRemoveSafe);
    c.answer(&first, Ok(json!({ "removed": true })));
    insta::assert_debug_snapshot!(buffer(&c, WIDE));
}

// ---------- clicks ----------

#[test]
fn the_cleanup_button_is_not_offered_without_a_workspace() {
    let c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(find(&buffer(&c, WIDE), "Cleanup"), None);
}

#[test]
fn the_cleanup_button_is_not_offered_while_disconnected_and_reconnecting_shows_instead() {
    let mut c = in_workspace();
    c.handle(ClientEvent::Disconnected);
    let buf = buffer(&c, WIDE);
    assert_eq!(find(&buf, "Cleanup"), None);
    assert!(find(&buf, "reconnecting…").is_some(), "{buf:?}");
}

#[test]
fn the_cleanup_button_hits_across_its_label_only() {
    let c = in_workspace();
    let button = at(&buffer(&c, WIDE), "Cleanup");
    let window = layout(WIDE);
    assert_eq!(
        hit(&window, c.client(), button.x, button.y),
        Some(Target::Cleanup)
    );
    assert_eq!(
        hit(&window, c.client(), button.x + 6, button.y),
        Some(Target::Cleanup)
    );
    assert_ne!(
        hit(&window, c.client(), button.x + 7, button.y),
        Some(Target::Cleanup)
    );
}

#[test]
fn clicking_remove_safe_removes_the_safe_rows_one_at_a_time() {
    let mut c = opened();
    let button = at(&buffer(&c, WIDE), "[Remove safe]");
    let (intents, sent) = click(&mut c, WIDE, button);
    assert_eq!(intents, [Intent::CleanupRemoveSafe]);
    let bodies: Vec<ClientRequest> = requests(&sent)
        .into_iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::WorktreeRemove(WorktreeRemove {
            path: "/wt/shepherd/old-idea".into(),
        })]
    );
}

#[test]
fn the_remove_button_goes_once_nothing_is_safe() {
    let mut c = opened();
    c.user(Intent::CleanupRemoveSafe);
    assert_eq!(find(&buffer(&c, WIDE), "[Remove safe]"), None);
}

#[test]
fn clicking_close_or_outside_closes_the_view_and_esc_does_too() {
    let mut c = opened();
    let close = at(&buffer(&c, WIDE), "[Close]");
    assert_eq!(click(&mut c, WIDE, close).0, [Intent::Dismiss]);
    assert_eq!(c.client().popup(), None);

    let mut c = opened();
    assert_eq!(
        click(&mut c, WIDE, Position::new(0, 0)).0,
        [Intent::Dismiss]
    );

    let c = opened();
    let esc = map_key(
        &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        c.client(),
        &Modes::default(),
    );
    assert_eq!(esc, [Intent::Dismiss]);
    let enter = map_key(
        &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        c.client(),
        &Modes::default(),
    );
    assert_eq!(enter, [], "only a click removes anything");
}

/// Twenty orphans in a short window: the wheel scrolls the view, and a row below the fold comes into view.
#[test]
fn the_wheel_scrolls_a_long_list() {
    let mut c = in_workspace();
    let asked = c.user(Intent::BeginCleanup);
    let many: Vec<Value> = (1..=20)
        .map(|i| {
            json!({ "path": format!("/wt/shepherd/w{i:02}"), "branch": format!("w{i:02}"),
                         "status": status(false, 0, true) })
        })
        .collect();
    let checks = c.answer(
        &asked,
        Ok(json!({ "worktrees": many, "cleanup_after_days": 7 })),
    );
    c.answer(&checks, Ok(json!({ "status": status(false, 1, false) })));
    assert_eq!(c.client().popup(), Some(&Popup::Cleanup));
    assert_eq!(find(&buffer(&c, SHORT), "w20"), None);
    let row = at(&buffer(&c, SHORT), "w01");
    let wheel = mouse(MouseEventKind::ScrollDown, row);
    let (intents, _) = events(&mut c, SHORT, &vec![wheel; 20]);
    assert!(matches!(intents[0], Intent::ScrollCleanup { by: 1, .. }));
    assert!(find(&buffer(&c, SHORT), "w20").is_some());
}

#[test]
fn a_huge_list_fits_the_window() {
    let mut c = in_workspace();
    let asked = c.user(Intent::BeginCleanup);
    let many: Vec<Value> = (0..70_000)
        .map(|i| json!({ "path": format!("/wt/x/w{i}"), "status": status(false, 0, true) }))
        .collect();
    c.answer(
        &asked,
        Ok(json!({ "worktrees": many, "cleanup_after_days": 7 })),
    );
    let buf = buffer(&c, WIDE);
    assert!(find(&buf, "[Remove safe]").is_some());
}
