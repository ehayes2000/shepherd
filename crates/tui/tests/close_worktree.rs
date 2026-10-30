//! Closing a session with a worktree (T25): the close confirmation says what removing the worktree would lose and has a
//! clickable answer for each choice, and a session whose worktree is being removed says so in the sidebar.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::{Value, json};
use shepherd_core::client::{ClientCommand, Intent};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{CloseWorktree, WireError};
use shepherd_term::Modes;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, render};

const WIDE: Rect = Rect::new(0, 0, 80, 24);

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// A caught-up client whose host has `/src/shepherd` selected and `s1`, named `refactor`, running in its worktree.
fn worktree_session() -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_live_with_bell(false);
    c.host().select_workspace("/src/shepherd");
    c.host().new_session("s1");
    c.host().rename("s1", "refactor");
    c.host().worktree_ready("s1");
    c.sync();
    c
}

/// [`worktree_session`] with its close confirmation open, its status request answered with `answer` (unanswered if
/// `None`).
fn confirming(answer: Option<Result<Value, WireError>>) -> ClientDriver {
    let mut c = worktree_session();
    let asked = c.user(Intent::BeginCloseSession(sid("s1")));
    if let Some(answer) = answer {
        c.answer(&asked, answer);
    }
    c
}

fn status(dirty: bool, ahead: u32, merged: bool) -> Option<Result<Value, WireError>> {
    Some(Ok(json!({
        "status": { "dirty": dirty, "ahead": ahead, "pushed": false, "merged_into_default": merged }
    })))
}

fn buffer(c: &ClientDriver, window: Rect) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(window.width, window.height)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(c.client(), &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

/// The buffer's text, a line per row.
fn text(buf: &Buffer) -> String {
    let mut lines = Vec::new();
    for y in buf.area.top()..buf.area.bottom() {
        let line: String = (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        lines.push(line.trim_end().to_owned());
    }
    lines.join("\n")
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

/// A press and a release at `p`: the intents of the release, applied to the client, and the commands they gave.
fn click(c: &mut ClientDriver, p: Position) -> (Vec<Intent>, Vec<ClientCommand>) {
    let mut pointer = Pointer::default();
    let modes = Modes::default();
    for intent in map_event(
        &mouse(MouseEventKind::Down(MouseButton::Left), p),
        c.client(),
        &modes,
        WIDE,
        &mut pointer,
    ) {
        c.user(intent);
    }
    let up = map_event(
        &mouse(MouseEventKind::Up(MouseButton::Left), p),
        c.client(),
        &modes,
        WIDE,
        &mut pointer,
    );
    let mut commands = Vec::new();
    for intent in &up {
        commands.extend(c.user(intent.clone()));
    }
    (up, commands)
}

fn key(c: &ClientDriver, code: KeyCode) -> Vec<Intent> {
    let event = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    map_event(
        &event,
        c.client(),
        &Modes::default(),
        WIDE,
        &mut Pointer::default(),
    )
}

// ---------- snapshots ----------

#[test]
fn close_confirmation_checking_the_worktree() {
    insta::assert_snapshot!(text(&buffer(&confirming(None), WIDE)));
}

#[test]
fn close_confirmation_of_a_worktree_that_loses_nothing() {
    insta::assert_snapshot!(text(&buffer(&confirming(status(false, 0, true)), WIDE)));
}

#[test]
fn close_confirmation_of_a_worktree_that_would_lose_work() {
    insta::assert_snapshot!(text(&buffer(&confirming(status(true, 2, false)), WIDE)));
}

#[test]
fn close_confirmation_of_a_worktree_already_gone() {
    insta::assert_snapshot!(text(&buffer(&confirming(Some(Ok(json!({})))), WIDE)));
}

#[test]
fn close_confirmation_of_a_worktree_that_could_not_be_checked() {
    let refused = WireError::InvalidParams("/wt/s1 isn't a worktree shepherd manages".into());
    insta::assert_snapshot!(text(&buffer(&confirming(Some(Err(refused))), WIDE)));
}

#[test]
fn a_session_removing_its_worktree() {
    let mut c = worktree_session();
    c.host()
        .close_with("s1", CloseWorktree::IfSafe, WorkspaceHooks::default());
    c.sync();
    insta::assert_snapshot!(text(&buffer(&c, Rect::new(0, 0, 80, 6))));
}

// ---------- clicks and keys ----------

#[test]
fn the_buttons_of_a_worktree_that_would_lose_work_are_hit_where_drawn() {
    let c = confirming(status(true, 2, false));
    let buf = buffer(&c, WIDE);
    let hit_at = |p: Position| hit(&layout(WIDE), c.client(), p.x, p.y);
    for (label, target) in [
        ("[Remove anyway]", Target::RemoveWorktree),
        ("[Keep worktree]", Target::KeepWorktree),
        ("[Cancel]", Target::Dismiss),
    ] {
        let start = at(&buf, label);
        let end = Position::new(start.x + label.chars().count() as u16 - 1, start.y);
        assert_eq!(hit_at(start), Some(target.clone()), "{label}");
        assert_eq!(hit_at(end), Some(target.clone()), "{label}");
    }
    assert_eq!(find(&buf, "[Close]"), None);
}

#[test]
fn clicking_remove_anyway_or_keep_worktree_closes_the_session_that_way() {
    for (label, intent, how) in [
        (
            "[Remove anyway]",
            Intent::CloseRemovingWorktree,
            CloseWorktree::Remove,
        ),
        (
            "[Keep worktree]",
            Intent::CloseKeepingWorktree,
            CloseWorktree::Keep,
        ),
    ] {
        let mut c = confirming(status(true, 2, false));
        let button = at(&buffer(&c, WIDE), label);
        let (intents, commands) = click(&mut c, button);
        assert_eq!(intents, [intent], "{label}");
        let sent = format!("{commands:?}");
        assert!(
            sent.contains(&format!("worktree: {how:?}")),
            "{label}: {sent}"
        );
        assert_eq!(c.client().popup(), None);
    }
}

#[test]
fn a_worktree_that_loses_nothing_is_closed_with_the_usual_buttons() {
    let c = confirming(status(false, 0, true));
    let buf = buffer(&c, WIDE);
    let close = at(&buf, "[Close]");
    assert_eq!(
        hit(&layout(WIDE), c.client(), close.x, close.y),
        Some(Target::Confirm)
    );
    assert_eq!(find(&buf, "[Remove anyway]"), None);
}

#[test]
fn the_keys_still_answer_a_close_confirmation_about_a_worktree() {
    let c = confirming(status(true, 2, false));
    assert_eq!(key(&c, KeyCode::Enter), [Intent::Confirm]);
    assert_eq!(key(&c, KeyCode::Esc), [Intent::Dismiss]);
}

#[test]
fn every_worktree_close_confirmation_draws_in_any_window_however_small() {
    let refused = WireError::InvalidParams("/wt/s1 isn't a worktree shepherd manages".into());
    let popups = [
        confirming(None),
        confirming(status(false, 0, true)),
        confirming(status(true, 2, false)),
        confirming(Some(Err(refused))),
    ];
    let mut panicked = Vec::new();
    for (i, c) in popups.iter().enumerate() {
        for (cols, rows) in (0..=8).flat_map(|w| (0..=8).map(move |h| (w * 7, h))) {
            let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buffer(c, Rect::new(0, 0, cols, rows))
            }));
            if drawn.is_err() {
                panicked.push(format!("{i} at {cols}x{rows}"));
            }
        }
    }
    assert_eq!(panicked, Vec::<String>::new());
}
