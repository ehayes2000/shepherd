//! `map_key` / `map_event`: the prefix key, its bindings, popups, and passthrough to the selected session.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use shepherd_core::client::{Cell, Focus, Intent, Popup};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::InputKind;
use shepherd_term::{Modes, MouseEncoding, MouseMode, MouseTracking};
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Pointer, layout, map_event, map_key};

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
        back_to_terminal_then(Intent::keys(vec![0x02]))
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
        back_to_terminal_then(Intent::keys(b"\x1b[98;5u".to_vec()))
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
        [Intent::keys(b"\x1b\x02".to_vec())]
    );
    let ctrl_shift_b = KeyEvent::new(
        KeyCode::Char('B'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(keys(&client(), ctrl_shift_b), [Intent::keys(vec![0x02])]);
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
    let answer = |bytes: &[u8]| [Intent::Input(InputKind::Answer, bytes.to_vec())];
    assert_eq!(keys(&client(), ch('a')), answer(b"a"));
    assert_eq!(keys(&client(), ctrl('c')), answer(&[0x03]));
    assert_eq!(keys(&client(), ch('q')), answer(b"q"));
}

#[test]
fn passthrough_follows_the_sessions_input_modes() {
    let app_cursor = Modes {
        app_cursor: true,
        ..Modes::default()
    };
    assert_eq!(
        map_key(&press(KeyCode::Up), client().client(), &app_cursor),
        [Intent::keys(b"\x1bOA".to_vec())]
    );
}

/// The kind of input `key` is sent to the session as, in `modes`.
fn kind_of(key: KeyEvent, modes: &Modes) -> InputKind {
    match map_key(&key, client().client(), modes).as_slice() {
        [Intent::Input(kind, _)] => *kind,
        other => panic!("{key:?} in {modes:?} sent {other:?}"),
    }
}

#[test]
fn keys_that_can_answer_a_dialog_are_marked_as_answers_in_every_keyboard_mode() {
    let kitty = |flags| Modes {
        kitty_keyboard: flags,
        ..Modes::default()
    };
    let modes = [Modes::default(), kitty(1), kitty(8), kitty(1 | 4 | 8)];
    let with = |code, mods| KeyEvent::new(code, mods);
    let answers = [
        press(KeyCode::Enter),
        press(KeyCode::Esc),
        ctrl('c'),
        ch('1'),
        ch('y'),
        ch('é'),
        with(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    ];
    let moves = [
        press(KeyCode::Up),
        press(KeyCode::Down),
        press(KeyCode::Tab),
        press(KeyCode::BackTab),
        press(KeyCode::Backspace),
        press(KeyCode::PageDown),
        press(KeyCode::F(1)),
        with(KeyCode::Enter, KeyModifiers::SHIFT),
        with(KeyCode::Esc, KeyModifiers::ALT),
        with(KeyCode::Char('y'), KeyModifiers::ALT),
        with(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        ctrl('d'),
    ];
    for modes in &modes {
        for key in answers {
            assert_eq!(
                kind_of(key, modes),
                InputKind::Answer,
                "{key:?} in {modes:?}"
            );
        }
        for key in moves {
            assert_eq!(kind_of(key, modes), InputKind::Keys, "{key:?} in {modes:?}");
        }
    }
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

#[test]
fn esc_dismisses_the_context_menu_and_other_keys_are_dropped() {
    let mut c = client();
    c.user(Intent::OpenMenu {
        session: SessionId::new("s1"),
        at: Cell { col: 2, row: 3 },
    });
    assert_eq!(keys(&c, press(KeyCode::Esc)), [Intent::Dismiss]);
    for key in [ch('r'), ch('x'), press(KeyCode::Enter), ctrl('b')] {
        assert_eq!(keys(&c, key), [], "{key:?}");
    }
}

#[test]
fn esc_dismisses_the_profile_picker_and_other_keys_are_dropped() {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("claude", AgentKind::ClaudeCode),
            ("shell", AgentKind::Shell),
        ],
        "claude",
    );
    c.user(Intent::BeginNewSession);
    assert_eq!(c.client().popup(), Some(&Popup::PickProfile));
    assert_eq!(keys(&c, press(KeyCode::Esc)), [Intent::Dismiss]);
    for key in [
        ch('s'),
        press(KeyCode::Enter),
        press(KeyCode::Down),
        ctrl('b'),
    ] {
        assert_eq!(keys(&c, key), [], "{key:?}");
    }
}

// ---------- other terminal events ----------

const WINDOW: Rect = Rect::new(0, 0, 100, 30);

fn events(c: &ClientDriver, event: Event, modes: Modes) -> Vec<Intent> {
    map_event(&event, c.client(), &modes, WINDOW, &mut Pointer::default())
}

#[test]
fn a_resize_reports_the_sessions_pane_size_and_how_many_sessions_the_sidebar_shows() {
    let pane = layout(Rect::new(0, 0, 120, 40)).pane;
    assert_eq!(
        events(&client(), Event::Resize(120, 40), Modes::default()),
        [
            Intent::Resize(PtySize {
                cols: pane.width,
                rows: pane.height
            }),
            // 40 lines: the three header lines, the footer, and 18 sessions of two lines.
            Intent::SidebarRows(18),
        ]
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
        [Intent::Input(
            InputKind::Paste,
            b"\x1b[200~hi\x1b[201~".to_vec()
        )]
    );
}

#[test]
fn a_paste_after_the_prefix_cancels_it_and_still_goes_through() {
    assert_eq!(
        events(&prefixed(), Event::Paste("hi".into()), Modes::default()),
        back_to_terminal_then(Intent::Input(InputKind::Paste, b"hi".to_vec()))
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
        [
            Intent::Input(InputKind::Focus, b"\x1b[I".to_vec()),
            Intent::WindowFocus(true)
        ]
    );
    assert_eq!(
        events(&client(), Event::FocusLost, Modes::default()),
        [Intent::WindowFocus(false)]
    );
}

#[test]
fn focus_changes_are_reported_to_shepherd_even_when_keys_do_not_go_to_the_session() {
    let focus = Modes {
        focus: true,
        ..Modes::default()
    };
    let mut helping = client();
    helping.user(Intent::ShowHelp);
    for c in [prefixed(), helping] {
        assert_eq!(
            events(&c, Event::FocusLost, focus),
            [Intent::WindowFocus(false)]
        );
        assert_eq!(
            events(&c, Event::FocusGained, focus),
            [Intent::WindowFocus(true)]
        );
    }
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
        [Intent::Input(InputKind::Mouse, b"\x1b[<0;4;3M".to_vec())]
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

// ---------- the workspace switcher ----------

/// The workspace switcher open with `path` typed.
fn switching(path: &str) -> ClientDriver {
    let mut c = client();
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath(path.to_owned()));
    c
}

#[test]
fn typing_in_the_workspace_switcher_edits_the_path() {
    let c = switching("/src/ap");
    assert_eq!(
        keys(&c, ch('p')),
        [Intent::EditWorkspacePath("/src/app".into())]
    );
    assert_eq!(
        keys(&c, press(KeyCode::Backspace)),
        [Intent::EditWorkspacePath("/src/a".into())]
    );
    assert_eq!(
        keys(&c, ctrl('u')),
        [Intent::EditWorkspacePath(String::new())]
    );
}

#[test]
fn enter_opens_the_typed_workspace_and_esc_dismisses_the_switcher() {
    let c = switching("/src/app");
    assert_eq!(keys(&c, press(KeyCode::Enter)), [Intent::Confirm]);
    assert_eq!(keys(&c, press(KeyCode::Esc)), [Intent::Dismiss]);
}

#[test]
fn a_paste_into_the_workspace_switcher_appends_its_first_line() {
    assert_eq!(
        events(
            &switching("/src/"),
            Event::Paste("app\nrest".into()),
            Modes::default()
        ),
        [Intent::EditWorkspacePath("/src/app".into())]
    );
}
