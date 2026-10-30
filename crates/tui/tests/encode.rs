//! Input encoding: keys, mouse, paste and focus turned into the bytes a program expects, following the input modes
//! its terminal has on (`Screen::modes()`).

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use shepherd_term::{Modes, MouseEncoding, MouseMode, MouseTracking};
use shepherd_tui::{encode, from_legacy_terminal};

const NONE: KeyModifiers = KeyModifiers::NONE;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;

fn legacy() -> Modes {
    Modes::default()
}

fn app_cursor() -> Modes {
    Modes {
        app_cursor: true,
        ..Modes::default()
    }
}

fn kitty(flags: u8) -> Modes {
    Modes {
        kitty_keyboard: flags,
        ..Modes::default()
    }
}

fn mouse_mode(tracking: MouseTracking, encoding: MouseEncoding) -> Modes {
    Modes {
        mouse: MouseMode { tracking, encoding },
        ..Modes::default()
    }
}

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn show(bytes: Option<Vec<u8>>) -> Option<String> {
    bytes.map(|b| String::from_utf8_lossy(&b).escape_debug().to_string())
}

#[track_caller]
fn check_keys(modes: Modes, table: &[(KeyCode, KeyModifiers, Option<&str>)]) {
    for (code, mods, want) in table {
        let got = encode::key(&key(*code, *mods), &modes);
        assert_eq!(
            show(got),
            want.map(|w| w.escape_debug().to_string()),
            "{code:?} with {mods:?} in {modes:?}"
        );
    }
}

#[test]
fn legacy_keys_encode_as_a_vt_terminal_sends_them() {
    use KeyCode::*;
    check_keys(
        legacy(),
        &[
            (Char('a'), NONE, Some("a")),
            (Char('A'), SHIFT, Some("A")),
            (Char('é'), NONE, Some("é")),
            (Char('a'), CTRL, Some("\x01")),
            (Char('z'), CTRL, Some("\x1a")),
            (Char('b'), CTRL, Some("\x02")),
            (Char(' '), CTRL, Some("\x00")),
            (Char('@'), CTRL, Some("\x00")),
            (Char('['), CTRL, Some("\x1b")),
            (Char('\\'), CTRL, Some("\x1c")),
            (Char(']'), CTRL, Some("\x1d")),
            (Char('^'), CTRL, Some("\x1e")),
            (Char('_'), CTRL, Some("\x1f")),
            (Char('?'), CTRL, Some("\x7f")),
            (Char('x'), ALT, Some("\x1bx")),
            (Char('c'), CTRL | ALT, Some("\x1b\x03")),
            (Enter, NONE, Some("\r")),
            (Enter, ALT, Some("\x1b\r")),
            // Shift+Enter has no legacy code of its own. It is sent as alt+Enter, which line editors and agent TUIs
            // take as "insert a newline", rather than as a plain Enter that submits.
            (Enter, SHIFT, Some("\x1b\r")),
            // A terminal that reports keys unambiguously sends ctrl plus a digit as such; VT terminals map them.
            (Char('2'), CTRL, Some("\x00")),
            (Char('3'), CTRL, Some("\x1b")),
            (Char('4'), CTRL, Some("\x1c")),
            (Char('5'), CTRL, Some("\x1d")),
            (Char('6'), CTRL, Some("\x1e")),
            (Char('7'), CTRL, Some("\x1f")),
            (Char('8'), CTRL, Some("\x7f")),
            (Char('/'), CTRL, Some("\x1f")),
            (Tab, NONE, Some("\t")),
            (BackTab, SHIFT, Some("\x1b[Z")),
            (Backspace, NONE, Some("\x7f")),
            (Backspace, CTRL, Some("\x08")),
            (Backspace, ALT, Some("\x1b\x7f")),
            (Esc, NONE, Some("\x1b")),
            (Up, NONE, Some("\x1b[A")),
            (Down, NONE, Some("\x1b[B")),
            (Right, NONE, Some("\x1b[C")),
            (Left, NONE, Some("\x1b[D")),
            (Up, SHIFT, Some("\x1b[1;2A")),
            (Left, CTRL, Some("\x1b[1;5D")),
            (Right, ALT, Some("\x1b[1;3C")),
            (Home, NONE, Some("\x1b[H")),
            (End, NONE, Some("\x1b[F")),
            (End, CTRL, Some("\x1b[1;5F")),
            (Insert, NONE, Some("\x1b[2~")),
            (Delete, NONE, Some("\x1b[3~")),
            (Delete, SHIFT, Some("\x1b[3;2~")),
            (PageUp, NONE, Some("\x1b[5~")),
            (PageDown, NONE, Some("\x1b[6~")),
            (PageDown, CTRL, Some("\x1b[6;5~")),
            (F(1), NONE, Some("\x1bOP")),
            (F(2), NONE, Some("\x1bOQ")),
            (F(3), NONE, Some("\x1bOR")),
            (F(4), NONE, Some("\x1bOS")),
            (F(1), SHIFT, Some("\x1b[1;2P")),
            (F(5), NONE, Some("\x1b[15~")),
            (F(6), NONE, Some("\x1b[17~")),
            (F(7), NONE, Some("\x1b[18~")),
            (F(8), NONE, Some("\x1b[19~")),
            (F(9), NONE, Some("\x1b[20~")),
            (F(10), NONE, Some("\x1b[21~")),
            (F(11), NONE, Some("\x1b[23~")),
            (F(12), NONE, Some("\x1b[24~")),
            (F(12), CTRL, Some("\x1b[24;5~")),
            (F(13), NONE, None),
            (CapsLock, NONE, None),
            (Null, NONE, None),
        ],
    );
}

#[test]
fn application_cursor_mode_sends_ss3_for_unmodified_arrows_home_and_end() {
    use KeyCode::*;
    check_keys(
        app_cursor(),
        &[
            (Up, NONE, Some("\x1bOA")),
            (Down, NONE, Some("\x1bOB")),
            (Right, NONE, Some("\x1bOC")),
            (Left, NONE, Some("\x1bOD")),
            (Home, NONE, Some("\x1bOH")),
            (End, NONE, Some("\x1bOF")),
            // A modifier needs the CSI form.
            (Up, CTRL, Some("\x1b[1;5A")),
        ],
    );
}

#[test]
fn kitty_disambiguate_sends_csi_u_for_keys_legacy_encoding_makes_ambiguous() {
    use KeyCode::*;
    check_keys(
        kitty(1),
        &[
            // Plain text stays text.
            (Char('a'), NONE, Some("a")),
            (Char('A'), SHIFT, Some("A")),
            (Enter, NONE, Some("\r")),
            (Tab, NONE, Some("\t")),
            (Backspace, NONE, Some("\x7f")),
            // Esc and modified keys get their own codes.
            (Esc, NONE, Some("\x1b[27u")),
            (Char('a'), CTRL, Some("\x1b[97;5u")),
            (Char('A'), CTRL | SHIFT, Some("\x1b[97;6u")),
            (Char('x'), ALT, Some("\x1b[120;3u")),
            (Char(' '), CTRL, Some("\x1b[32;5u")),
            (Enter, SHIFT, Some("\x1b[13;2u")),
            (Tab, CTRL, Some("\x1b[9;5u")),
            (BackTab, SHIFT, Some("\x1b[9;2u")),
            (Backspace, ALT, Some("\x1b[127;3u")),
            // Cursor and function keys keep their legacy forms, except F3, whose `CSI R` reads as a cursor report.
            (Up, NONE, Some("\x1b[A")),
            (Up, SHIFT, Some("\x1b[1;2A")),
            (PageUp, NONE, Some("\x1b[5~")),
            (F(1), NONE, Some("\x1bOP")),
            (F(3), NONE, Some("\x1b[13~")),
            (F(3), CTRL, Some("\x1b[13;5~")),
        ],
    );
}

#[test]
fn kitty_report_all_keys_sends_every_key_as_csi_u() {
    use KeyCode::*;
    check_keys(
        kitty(1 | 8),
        &[
            (Char('a'), NONE, Some("\x1b[97u")),
            (Char('A'), SHIFT, Some("\x1b[97;2u")),
            (Enter, NONE, Some("\x1b[13u")),
            (Tab, NONE, Some("\x1b[9u")),
            (Backspace, NONE, Some("\x1b[127u")),
            (Esc, NONE, Some("\x1b[27u")),
            (Up, NONE, Some("\x1b[A")),
        ],
    );
}

#[test]
fn kitty_alternate_keys_adds_the_shifted_key() {
    use KeyCode::*;
    check_keys(
        kitty(1 | 4),
        &[
            (Char('A'), CTRL | SHIFT, Some("\x1b[97:65;6u")),
            // Without shift there is no alternate.
            (Char('a'), CTRL, Some("\x1b[97;5u")),
        ],
    );
}

#[test]
fn kitty_event_types_leave_presses_unchanged() {
    use KeyCode::*;
    check_keys(
        kitty(1 | 2),
        &[
            (Char('a'), CTRL, Some("\x1b[97;5u")),
            (Esc, NONE, Some("\x1b[27u")),
        ],
    );
}

fn mouse(kind: MouseEventKind, mods: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: 99,
        row: 99,
        modifiers: mods,
    }
}

#[track_caller]
fn check_mouse(
    modes: Modes,
    at: (u16, u16),
    table: &[(MouseEventKind, KeyModifiers, Option<&str>)],
) {
    for (kind, mods, want) in table {
        let got = encode::mouse(&mouse(*kind, *mods), at.0, at.1, &modes);
        assert_eq!(
            show(got),
            want.map(|w| w.escape_debug().to_string()),
            "{kind:?} with {mods:?} at {at:?} in {modes:?}"
        );
    }
}

#[test]
fn mouse_is_dropped_when_the_program_has_not_asked_for_it() {
    use MouseEventKind::*;
    check_mouse(
        legacy(),
        (0, 0),
        &[
            (Down(MouseButton::Left), NONE, None),
            (ScrollUp, NONE, None),
        ],
    );
}

#[test]
fn sgr_mouse_reports_presses_releases_and_the_wheel_with_one_based_coordinates() {
    use MouseEventKind::*;
    check_mouse(
        mouse_mode(MouseTracking::Click, MouseEncoding::Sgr),
        (4, 2),
        &[
            (Down(MouseButton::Left), NONE, Some("\x1b[<0;5;3M")),
            (Down(MouseButton::Middle), NONE, Some("\x1b[<1;5;3M")),
            (Down(MouseButton::Right), NONE, Some("\x1b[<2;5;3M")),
            (Up(MouseButton::Right), NONE, Some("\x1b[<2;5;3m")),
            (ScrollUp, NONE, Some("\x1b[<64;5;3M")),
            (ScrollDown, NONE, Some("\x1b[<65;5;3M")),
            (ScrollLeft, NONE, Some("\x1b[<66;5;3M")),
            (ScrollRight, NONE, Some("\x1b[<67;5;3M")),
            (Down(MouseButton::Left), SHIFT, Some("\x1b[<4;5;3M")),
            (Down(MouseButton::Left), ALT, Some("\x1b[<8;5;3M")),
            (Down(MouseButton::Left), CTRL, Some("\x1b[<16;5;3M")),
            // Click tracking doesn't report motion.
            (Drag(MouseButton::Left), NONE, None),
            (Moved, NONE, None),
        ],
    );
}

#[test]
fn drag_tracking_adds_motion_with_a_button_held() {
    use MouseEventKind::*;
    check_mouse(
        mouse_mode(MouseTracking::Drag, MouseEncoding::Sgr),
        (0, 0),
        &[
            (Drag(MouseButton::Left), NONE, Some("\x1b[<32;1;1M")),
            (Drag(MouseButton::Right), NONE, Some("\x1b[<34;1;1M")),
            (Moved, NONE, None),
        ],
    );
}

#[test]
fn motion_tracking_adds_every_motion() {
    use MouseEventKind::*;
    check_mouse(
        mouse_mode(MouseTracking::Motion, MouseEncoding::Sgr),
        (0, 0),
        &[(Moved, NONE, Some("\x1b[<35;1;1M"))],
    );
}

#[test]
fn x10_mouse_offsets_everything_by_32_and_releases_as_button_3() {
    use MouseEventKind::*;
    let modes = mouse_mode(MouseTracking::Click, MouseEncoding::X10);
    check_mouse(
        modes,
        (4, 2),
        &[
            (Down(MouseButton::Left), NONE, Some("\x1b[M %#")),
            (Up(MouseButton::Left), NONE, Some("\x1b[M#%#")),
            (ScrollDown, NONE, Some("\x1b[Ma%#")),
        ],
    );
    // Past column 223 the position doesn't fit in a byte.
    check_mouse(modes, (223, 0), &[(Down(MouseButton::Left), NONE, None)]);
}

#[test]
fn utf8_mouse_encodes_large_coordinates_as_utf8() {
    let modes = mouse_mode(MouseTracking::Click, MouseEncoding::Utf8);
    let got = encode::mouse(
        &mouse(MouseEventKind::Down(MouseButton::Left), NONE),
        299,
        0,
        &modes,
    );
    let mut want = b"\x1b[M ".to_vec();
    want.extend("\u{14c}!".as_bytes()); // 32 + 300, 32 + 1
    assert_eq!(got, Some(want));
}

#[test]
fn bracketed_paste_wraps_the_text_and_strips_an_embedded_end_marker() {
    let modes = Modes {
        bracketed_paste: true,
        ..Modes::default()
    };
    // Every ESC goes, as xterm does, so no end marker can survive or be put back together.
    assert_eq!(
        encode::paste("a\nb\x1b[201~c", &modes),
        b"\x1b[200~a\nb[201~c\x1b[201~".to_vec()
    );
    assert_eq!(
        encode::paste("\x1b[20\x1b[201~1~", &modes),
        b"\x1b[200~[20[201~1~\x1b[201~".to_vec()
    );
}

/// What crossterm reports for bytes 0x1C to 0x1F from a terminal that doesn't report keys unambiguously
/// (`event/sys/unix/parse.rs`): ctrl plus the digits 4 to 7. `from_legacy_terminal` turns them back into the
/// keys usually pressed for them.
fn crossterm_legacy(byte: u8) -> KeyEvent {
    KeyEvent::new(KeyCode::Char((byte - 0x1c + b'4') as char), CTRL)
}

#[test]
fn control_bytes_1c_to_1f_from_a_legacy_terminal_reach_the_session_as_those_control_keys() {
    let table = [
        // byte, legacy, kitty
        (0x1c, "\x1c", "\x1b[92;5u"), // ctrl-\
        (0x1d, "\x1d", "\x1b[93;5u"), // ctrl-]
        (0x1e, "\x1e", "\x1b[94;5u"), // ctrl-^
        (0x1f, "\x1f", "\x1b[47;5u"), // ctrl-/
    ];
    for (byte, legacy_bytes, kitty_bytes) in table {
        let key = from_legacy_terminal(crossterm_legacy(byte));
        assert_eq!(
            show(encode::key(&key, &legacy())),
            Some(legacy_bytes.escape_debug().to_string()),
            "{byte:#x} legacy"
        );
        assert_eq!(
            show(encode::key(&key, &kitty(1))),
            Some(kitty_bytes.escape_debug().to_string()),
            "{byte:#x} kitty"
        );
    }
}

#[test]
fn other_keys_from_a_legacy_terminal_are_left_alone() {
    for key in [
        KeyEvent::new(KeyCode::Char('4'), NONE),
        KeyEvent::new(KeyCode::Char('a'), CTRL),
        KeyEvent::new(KeyCode::Char('4'), CTRL | ALT),
        KeyEvent::new(KeyCode::Enter, SHIFT),
    ] {
        assert_eq!(from_legacy_terminal(key), key, "{key:?}");
    }
}

#[test]
fn plain_paste_sends_newlines_as_carriage_returns() {
    assert_eq!(encode::paste("a\r\nb\nc", &legacy()), b"a\rb\rc".to_vec());
}

#[test]
fn focus_reports_are_sent_only_when_the_program_asked_for_them() {
    let on = Modes {
        focus: true,
        ..Modes::default()
    };
    assert_eq!(encode::focus(true, &on), Some(b"\x1b[I".to_vec()));
    assert_eq!(encode::focus(false, &on), Some(b"\x1b[O".to_vec()));
    assert_eq!(encode::focus(true, &legacy()), None);
}
