//! `map_key` / `map_event`: the prefix key, its bindings, popups, and passthrough to the selected session.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use shepherd_core::client::{Focus, Intent};
use shepherd_core::ports::pty::PtySize;
use shepherd_term::{Modes, MouseEncoding, MouseMode, MouseTracking};
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{layout, map_event, map_key};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn ch(c: char) -> KeyEvent {
    press(KeyCode::Char(c))
}

/// A connected client with two sessions, keys going to the terminal.
fn client() -> ClientDriver {
    ClientDriver::with_sessions(&["s1", "s2"])
}

/// The same, after the prefix key.
fn prefixed() -> ClientDriver {
    let mut c = client();
    c.user(Intent::SetFocus(Focus::Command));
    c
}

fn keys(c: &ClientDriver, key: KeyEvent) -> Vec<Intent> {
    map_key(&key, c.client(), &Modes::default())
}

fn back_to_terminal_then(intent: Intent) -> Vec<Intent> {
    vec![Intent::SetFocus(Focus::Terminal), intent]
}

// ---------- the prefix ----------

#[test]
fn the_prefix_key_moves_focus_to_shepherds_commands() {
    assert_eq!(
        keys(&client(), ctrl('b')),
        [Intent::SetFocus(Focus::Command)]
    );
}

#[test]
fn each_prefix_binding_runs_its_command_and_returns_keys_to_the_terminal() {
    let table = [
        (ch('n'), Intent::NewSession),
        (ch('j'), Intent::NextSession),
        (press(KeyCode::Down), Intent::NextSession),
        (ch('k'), Intent::PrevSession),
        (press(KeyCode::Up), Intent::PrevSession),
        (ch('r'), Intent::BeginRename),
        (ch('x'), Intent::BeginClose),
        (ch('?'), Intent::ShowHelp),
        (ch('q'), Intent::Quit),
    ];
    for (key, intent) in table {
        assert_eq!(
            keys(&prefixed(), key),
            back_to_terminal_then(intent),
            "{key:?}"
        );
    }
}

#[test]
fn the_prefix_twice_sends_a_literal_prefix_to_the_terminal() {
    assert_eq!(
        keys(&prefixed(), ctrl('b')),
        back_to_terminal_then(Intent::Input(vec![0x02]))
    );
}

#[test]
fn the_literal_prefix_follows_the_sessions_keyboard_mode() {
    let kitty = Modes {
        kitty_keyboard: 1,
        ..Modes::default()
    };
    assert_eq!(
        map_key(&ctrl('b'), prefixed().client(), &kitty),
        back_to_terminal_then(Intent::Input(b"\x1b[98;5u".to_vec()))
    );
}

#[test]
fn a_binding_with_ctrl_or_alt_after_the_prefix_is_not_that_binding() {
    for key in [
        ctrl('n'),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT),
    ] {
        assert_eq!(
            keys(&prefixed(), key),
            [Intent::SetFocus(Focus::Terminal)],
            "{key:?}"
        );
    }
    // Shift is how `?` is typed, so it doesn't count.
    let question = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
    assert_eq!(
        keys(&prefixed(), question),
        back_to_terminal_then(Intent::ShowHelp)
    );
}

#[test]
fn only_ctrl_b_alone_is_the_prefix() {
    let ctrl_alt_b = KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    assert_eq!(
        keys(&client(), ctrl_alt_b),
        [Intent::Input(b"\x1b\x02".to_vec())]
    );
    let ctrl_shift_b = KeyEvent::new(
        KeyCode::Char('B'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(keys(&client(), ctrl_shift_b), [Intent::Input(vec![0x02])]);
}

#[test]
fn an_unbound_key_after_the_prefix_cancels_it_and_is_dropped() {
    for key in [ch('z'), press(KeyCode::Esc)] {
        assert_eq!(
            keys(&prefixed(), key),
            [Intent::SetFocus(Focus::Terminal)],
            "{key:?}"
        );
    }
}

// ---------- passthrough ----------

#[test]
fn keys_without_the_prefix_go_to_the_terminal_encoded() {
    assert_eq!(keys(&client(), ch('a')), [Intent::Input(b"a".to_vec())]);
    assert_eq!(keys(&client(), ctrl('c')), [Intent::Input(vec![0x03])]);
    assert_eq!(keys(&client(), ch('q')), [Intent::Input(b"q".to_vec())]);
}

#[test]
fn passthrough_follows_the_sessions_input_modes() {
    let app_cursor = Modes {
        app_cursor: true,
        ..Modes::default()
    };
    assert_eq!(
        map_key(&press(KeyCode::Up), client().client(), &app_cursor),
        [Intent::Input(b"\x1bOA".to_vec())]
    );
}

#[test]
fn key_releases_are_ignored() {
    let mut release = ch('a');
    release.kind = KeyEventKind::Release;
    assert_eq!(keys(&client(), release), []);
    let mut release = ctrl('b');
    release.kind = KeyEventKind::Release;
    assert_eq!(keys(&client(), release), []);
}

// ---------- popups ----------

fn renaming(name: &str) -> ClientDriver {
    let mut c = client();
    c.user(Intent::BeginRename);
    c.user(Intent::EditRename(name.to_owned()));
    c
}

#[test]
fn typing_in_the_rename_popup_edits_the_name() {
    let c = renaming("ap");
    assert_eq!(keys(&c, ch('i')), [Intent::EditRename("api".into())]);
    assert_eq!(
        keys(&c, KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT)),
        [Intent::EditRename("apI".into())]
    );
    assert_eq!(
        keys(&c, press(KeyCode::Backspace)),
        [Intent::EditRename("a".into())]
    );
    assert_eq!(keys(&c, ctrl('u')), [Intent::EditRename(String::new())]);
}

#[test]
fn backspace_removes_a_whole_character() {
    let c = renaming("café");
    assert_eq!(
        keys(&c, press(KeyCode::Backspace)),
        [Intent::EditRename("caf".into())]
    );
}

#[test]
fn enter_accepts_the_rename_and_esc_dismisses_it() {
    let c = renaming("api");
    assert_eq!(keys(&c, press(KeyCode::Enter)), [Intent::Confirm]);
    assert_eq!(keys(&c, press(KeyCode::Esc)), [Intent::Dismiss]);
}

#[test]
fn the_prefix_is_text_while_renaming() {
    // The popup holds the keys; ctrl-b doesn't start a command under it.
    assert_eq!(keys(&renaming("a"), ctrl('b')), []);
}

#[test]
fn the_close_confirmation_takes_y_or_enter_and_n_or_esc() {
    let mut c = client();
    c.user(Intent::BeginClose);
    for key in [ch('y'), ch('Y'), press(KeyCode::Enter)] {
        assert_eq!(keys(&c, key), [Intent::Confirm], "{key:?}");
    }
    for key in [ch('n'), ch('N'), press(KeyCode::Esc)] {
        assert_eq!(keys(&c, key), [Intent::Dismiss], "{key:?}");
    }
    assert_eq!(keys(&c, ch('a')), []);
}

#[test]
fn any_key_closes_the_help() {
    let mut c = client();
    c.user(Intent::ShowHelp);
    for key in [ch('q'), press(KeyCode::Esc), ch('?'), ch('a')] {
        assert_eq!(keys(&c, key), [Intent::Dismiss], "{key:?}");
    }
}

// ---------- other terminal events ----------

const WINDOW: Rect = Rect::new(0, 0, 100, 30);

fn events(c: &ClientDriver, event: Event, modes: Modes) -> Vec<Intent> {
    map_event(&event, c.client(), &modes, WINDOW)
}

#[test]
fn a_resize_reports_the_sessions_pane_size() {
    let pane = layout(Rect::new(0, 0, 120, 40)).pane;
    assert_eq!(
        events(&client(), Event::Resize(120, 40), Modes::default()),
        [Intent::Resize(PtySize {
            cols: pane.width,
            rows: pane.height
        })]
    );
}

#[test]
fn a_paste_goes_to_the_terminal_encoded() {
    let bracketed = Modes {
        bracketed_paste: true,
        ..Modes::default()
    };
    assert_eq!(
        events(&client(), Event::Paste("hi".into()), bracketed),
        [Intent::Input(b"\x1b[200~hi\x1b[201~".to_vec())]
    );
}

#[test]
fn a_paste_after_the_prefix_cancels_it_and_still_goes_through() {
    assert_eq!(
        events(&prefixed(), Event::Paste("hi".into()), Modes::default()),
        back_to_terminal_then(Intent::Input(b"hi".to_vec()))
    );
}

#[test]
fn a_paste_into_the_rename_popup_appends_its_first_line() {
    assert_eq!(
        events(
            &renaming("a"),
            Event::Paste("pi\nrest".into()),
            Modes::default()
        ),
        [Intent::EditRename("api".into())]
    );
}

#[test]
fn focus_changes_are_reported_when_the_session_asked() {
    let focus = Modes {
        focus: true,
        ..Modes::default()
    };
    assert_eq!(
        events(&client(), Event::FocusGained, focus),
        [Intent::Input(b"\x1b[I".to_vec())]
    );
    assert_eq!(events(&client(), Event::FocusLost, Modes::default()), []);
}

fn click(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
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

#[test]
fn mouse_in_the_pane_goes_to_the_session_in_its_own_coordinates() {
    let pane = layout(WINDOW).pane;
    assert_eq!(
        events(&client(), click(pane.x + 3, 2), sgr_mouse()),
        [Intent::Input(b"\x1b[<0;4;3M".to_vec())]
    );
}

#[test]
fn mouse_outside_the_pane_or_untracked_is_dropped() {
    assert_eq!(events(&client(), click(0, 0), sgr_mouse()), []);
    let pane = layout(WINDOW).pane;
    assert_eq!(events(&client(), click(pane.x, 0), Modes::default()), []);
}

#[test]
fn mouse_is_dropped_under_a_popup_or_after_the_prefix() {
    let pane = layout(WINDOW).pane;
    assert_eq!(events(&prefixed(), click(pane.x, 0), sgr_mouse()), []);
    let mut c = client();
    c.user(Intent::ShowHelp);
    assert_eq!(events(&c, click(pane.x, 0), sgr_mouse()), []);
}
