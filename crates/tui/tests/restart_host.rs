//! A host of another build (T36a): the restart dialog says how many sessions restarting stops and has clickable
//! answers, and the notices that say what to do instead fit on screen.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::{Value, json};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{Client, ClientCommand, ClientEvent, Intent};
use shepherd_core::ids::MachineId;
use shepherd_core::wire::{BuildId, ExeDigest, PROTOCOL_VERSION, WireError};
use shepherd_term::Modes;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, render};

const WIDE: Rect = Rect::new(0, 0, 80, 24);

fn build(byte: u8) -> BuildId {
    BuildId {
        version: "0.1.0".into(),
        digest: ExeDigest::from_bytes([byte; 32]),
    }
}

fn digest_json(byte: u8) -> Value {
    serde_json::to_value(ExeDigest::from_bytes([byte; 32])).unwrap_or_else(|e| panic!("{e}"))
}

/// A client of build 1 whose hello is answered with `answer`, then (if the hello succeeded) caught up with a host
/// running `fix-login` and `docs`, and an exited `old`.
fn answered(answer: Result<Value, WireError>) -> ClientDriver {
    let mut c =
        ClientDriver::from_client(Client::new(MachineId::new("m_client")).with_build(build(1)));
    c.handle(ClientEvent::Start);
    let hello = c.handle(ClientEvent::Connected {
        at: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap_or_else(|e| panic!("{e}")),
    });
    let ok = answer.is_ok();
    let subscribe = c.answer(&hello, answer);
    if ok {
        for (id, name) in [("s1", "old"), ("s2", "fix-login"), ("s3", "docs")] {
            c.host().new_session(id);
            c.host().rename(id, name);
        }
        c.host().pty_exited("s1", Some(0));
        c.sync();
        c.answer(&subscribe, Ok(json!({})));
    }
    c
}

/// A host running build `running` (none if `None`) with build `installed` at its path.
fn hello_ok(running: Option<u8>, installed: Option<u8>) -> Result<Value, WireError> {
    let mut answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test" });
    if let Some(b) = running {
        answer["build"] = serde_json::to_value(build(b)).unwrap_or_else(|e| panic!("{e}"));
    }
    if let Some(b) = installed {
        answer["installed"] = digest_json(b);
    }
    Ok(answer)
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

fn at(buf: &Buffer, text: &str) -> Position {
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

fn mouse(kind: MouseEventKind, p: Position) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: p.x,
        row: p.y,
        modifiers: KeyModifiers::NONE,
    })
}

/// A press and a release at `p`, applied to the client: the commands they gave.
fn click(c: &mut ClientDriver, p: Position) -> Vec<ClientCommand> {
    let mut pointer = Pointer::default();
    let modes = Modes::default();
    let mut commands = Vec::new();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        for intent in map_event(&mouse(kind, p), c.client(), &modes, WIDE, &mut pointer) {
            commands.extend(c.user(intent));
        }
    }
    commands
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
fn restart_dialog_for_a_host_older_than_builds() {
    insta::assert_snapshot!(text(&buffer(&answered(hello_ok(None, None)), WIDE)));
}

#[test]
fn restart_dialog_for_a_stale_host() {
    insta::assert_snapshot!(text(&buffer(&answered(hello_ok(Some(2), Some(1))), WIDE)));
}

#[test]
fn restart_dialog_for_a_host_of_another_protocol_version() {
    let c = answered(Err(WireError::VersionMismatch { host: 2, client: 1 }));
    insta::assert_snapshot!(text(&buffer(&c, WIDE)));
}

#[test]
fn notice_for_a_window_of_another_build_than_the_installed_host() {
    insta::assert_snapshot!(text(&buffer(&answered(hello_ok(Some(2), Some(2))), WIDE)));
}

#[test]
fn notice_for_a_method_the_host_does_not_know() {
    let mut c = answered(hello_ok(Some(1), Some(1)));
    c.user(Intent::BeginRename);
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(WireError::MethodNotFound("_shepherd/session/rename".into())),
    );
    insta::assert_snapshot!(text(&buffer(&c, WIDE)));
}

// ---------- clicks and keys ----------

#[test]
fn the_restart_dialogs_buttons_are_hit_where_drawn() {
    let c = answered(hello_ok(None, None));
    let buf = buffer(&c, WIDE);
    let hit_at = |p: Position| hit(&layout(WIDE), c.client(), p.x, p.y);
    for (label, target) in [
        ("[Restart host]", Target::RestartHost),
        ("[Not now]", Target::Dismiss),
    ] {
        let start = at(&buf, label);
        let end = Position::new(start.x + label.chars().count() as u16 - 1, start.y);
        assert_eq!(hit_at(start), Some(target.clone()), "{label}");
        assert_eq!(hit_at(end), Some(target), "{label}");
    }
}

#[test]
fn clicking_restart_host_terminates_the_host() {
    let mut c = answered(hello_ok(None, None));
    let button = at(&buffer(&c, WIDE), "[Restart host]");
    assert_eq!(click(&mut c, button), [ClientCommand::TerminateHost]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn clicking_not_now_closes_the_dialog_without_terminating() {
    let mut c = answered(hello_ok(None, None));
    let button = at(&buffer(&c, WIDE), "[Not now]");
    assert_eq!(click(&mut c, button), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn enter_does_not_restart_and_esc_closes_the_dialog() {
    let c = answered(hello_ok(None, None));
    assert_eq!(key(&c, KeyCode::Enter), []);
    assert_eq!(key(&c, KeyCode::Char('y')), []);
    assert_eq!(key(&c, KeyCode::Esc), [Intent::Dismiss]);
}
