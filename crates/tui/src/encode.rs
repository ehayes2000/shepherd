//! Input encoding: turns keys, mouse events, pastes and focus changes into the bytes the program in a session
//! expects, following the input modes its terminal has on ([`shepherd_term::Screen::modes`]).
//!
//! - Keys use the VT/xterm legacy encoding, with `SS3` cursor keys in application cursor mode (DECCKM). When the
//!   program has turned on the kitty keyboard protocol, flag 1 (disambiguate) sends `Esc` and keys with ctrl, alt or
//!   super as `CSI code;mods u`, flag 8 (report all keys) does that for every key, and flag 4 (alternate keys) adds the
//!   shifted key. Flag 2 (event types) changes nothing for presses, the only kind of key event the outer terminal
//!   reports to us. Flag 16 (associated text) is not encoded.
//! - Mouse events are dropped unless the program asked for them, and are then filtered by its tracking level
//!   (click, drag, motion) and encoded as X10, UTF-8 or SGR.
//! - Pastes are wrapped in `CSI 200~ … CSI 201~` in bracketed paste mode, with any end marker inside the text removed.
//!   Otherwise newlines are sent as carriage returns, as a terminal does.
//! - Focus changes are reported (`CSI I` / `CSI O`) only when the program asked for them.

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use shepherd_term::{Modes, MouseEncoding, MouseTracking};

const KITTY_DISAMBIGUATE: u8 = 1;
const KITTY_ALTERNATE_KEYS: u8 = 4;
const KITTY_ALL_KEYS: u8 = 8;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The bytes for `key`, or `None` for a key with no encoding (or a release).
pub fn key(key: &KeyEvent, modes: &Modes) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let mods = key.modifiers;
    let kitty = modes.kitty_keyboard & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0;
    match key.code {
        KeyCode::Char(c) if kitty => Some(kitty_char(c, mods, modes.kitty_keyboard)),
        KeyCode::Char(c) => Some(legacy_char(c, mods)),
        KeyCode::Enter => Some(text_key(13, b"\r", mods, modes)),
        KeyCode::Tab => Some(text_key(9, b"\t", mods, modes)),
        KeyCode::Backspace if !kitty && mods.contains(KeyModifiers::CONTROL) => {
            Some(alt_prefixed(b"\x08", mods))
        }
        KeyCode::Backspace => Some(text_key(127, b"\x7f", mods, modes)),
        KeyCode::BackTab if kitty => Some(csi_u(9, None, param(mods | KeyModifiers::SHIFT))),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc if kitty => Some(csi_u(27, None, param(mods))),
        KeyCode::Esc => Some(alt_prefixed(b"\x1b", mods)),
        KeyCode::Up => Some(cursor_key(b'A', mods, modes)),
        KeyCode::Down => Some(cursor_key(b'B', mods, modes)),
        KeyCode::Right => Some(cursor_key(b'C', mods, modes)),
        KeyCode::Left => Some(cursor_key(b'D', mods, modes)),
        KeyCode::Home => Some(cursor_key(b'H', mods, modes)),
        KeyCode::End => Some(cursor_key(b'F', mods, modes)),
        KeyCode::Insert => Some(tilde_key(2, mods)),
        KeyCode::Delete => Some(tilde_key(3, mods)),
        KeyCode::PageUp => Some(tilde_key(5, mods)),
        KeyCode::PageDown => Some(tilde_key(6, mods)),
        KeyCode::F(3) if kitty => Some(tilde_key(13, mods)),
        KeyCode::F(n @ 1..=4) => Some(ss3_key(b"PQRS"[usize::from(n - 1)], mods)),
        KeyCode::F(n @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            Some(tilde_key(CODES[usize::from(n - 5)], mods))
        }
        _ => None,
    }
}

/// The bytes for a mouse event at `col`, `row` (0-based, in the session's own grid), or `None` if the program
/// doesn't track this kind of event or the position can't be encoded.
pub fn mouse(event: &MouseEvent, col: u16, row: u16, modes: &Modes) -> Option<Vec<u8>> {
    let tracking = modes.mouse.tracking;
    if tracking == MouseTracking::Off {
        return None;
    }
    let drag = matches!(tracking, MouseTracking::Drag | MouseTracking::Motion);
    let (code, release) = match event.kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) if drag => (button(b) + 32, false),
        MouseEventKind::Moved if tracking == MouseTracking::Motion => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        MouseEventKind::Drag(_) | MouseEventKind::Moved => return None,
    };
    let mods = event.modifiers;
    let mut mod_bits = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, 4),
        (KeyModifiers::ALT, 8),
        (KeyModifiers::CONTROL, 16),
    ] {
        if mods.contains(flag) {
            mod_bits |= bit;
        }
    }
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match modes.mouse.encoding {
        MouseEncoding::Sgr => {
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{x};{y}{end}", code | mod_bits).into_bytes())
        }
        encoding => {
            // The legacy encodings can't say which button was released.
            let code = if release { 3 } else { code } | mod_bits;
            let mut out = b"\x1b[M".to_vec();
            out.push(32 + code);
            for n in [x, y] {
                let n = 32 + n;
                match encoding {
                    MouseEncoding::Utf8 => {
                        let c = char::from_u32(n).filter(|_| n < 2048)?;
                        out.extend(c.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                    _ => out.push(u8::try_from(n).ok()?),
                }
            }
            Some(out)
        }
    }
}

/// The bytes for pasting `text`.
pub fn paste(text: &str, modes: &Modes) -> Vec<u8> {
    if modes.bracketed_paste {
        let inner = text.replace(PASTE_END, "");
        format!("{PASTE_START}{inner}{PASTE_END}").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// The report for the terminal gaining (`true`) or losing focus, if the program asked for one.
pub fn focus(gained: bool, modes: &Modes) -> Option<Vec<u8>> {
    modes
        .focus
        .then(|| if gained { b"\x1b[I" } else { b"\x1b[O" }.to_vec())
}

fn button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// The xterm / kitty modifier parameter: 1 plus a bit per modifier.
fn param(mods: KeyModifiers) -> u8 {
    let mut bits = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, 1),
        (KeyModifiers::ALT, 2),
        (KeyModifiers::CONTROL, 4),
        (KeyModifiers::SUPER, 8),
        (KeyModifiers::HYPER, 16),
        (KeyModifiers::META, 32),
    ] {
        if mods.contains(flag) {
            bits |= bit;
        }
    }
    1 + bits
}

fn csi_u(code: u32, shifted: Option<u32>, param: u8) -> Vec<u8> {
    let mut out = format!("\x1b[{code}");
    if let Some(shifted) = shifted {
        out.push_str(&format!(":{shifted}"));
    }
    if param > 1 {
        out.push_str(&format!(";{param}"));
    }
    out.push('u');
    out.into_bytes()
}

fn kitty_char(c: char, mods: KeyModifiers, flags: u8) -> Vec<u8> {
    let command = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    if !command && flags & KITTY_ALL_KEYS == 0 {
        return c.to_string().into_bytes();
    }
    let base = c.to_ascii_lowercase();
    let shifted =
        (flags & KITTY_ALTERNATE_KEYS != 0 && mods.contains(KeyModifiers::SHIFT) && base != c)
            .then_some(u32::from(c));
    csi_u(u32::from(base), shifted, param(mods))
}

fn legacy_char(c: char, mods: KeyModifiers) -> Vec<u8> {
    let ctrl = mods
        .contains(KeyModifiers::CONTROL)
        .then(|| ctrl_byte(c))
        .flatten();
    let bytes = match ctrl {
        Some(b) => vec![b],
        None => c.to_string().into_bytes(),
    };
    alt_prefixed(&bytes, mods)
}

/// What ctrl plus `c` sends in a legacy terminal, if it has a control code.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' | 'A'..='Z' => Some(c.to_ascii_lowercase() as u8 & 0x1f),
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn alt_prefixed(bytes: &[u8], mods: KeyModifiers) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if mods.contains(KeyModifiers::ALT) {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// Enter, Tab and Backspace: their control byte, or a `CSI u` code in kitty mode when modified (or always, with
/// report all keys).
fn text_key(code: u32, legacy: &[u8], mods: KeyModifiers, modes: &Modes) -> Vec<u8> {
    let flags = modes.kitty_keyboard;
    let kitty = flags & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0;
    if kitty && (flags & KITTY_ALL_KEYS != 0 || !mods.is_empty()) {
        csi_u(code, None, param(mods))
    } else {
        alt_prefixed(legacy, mods)
    }
}

/// Arrows, Home and End: `CSI x`, `SS3 x` in application cursor mode, or `CSI 1;mods x` when modified.
fn cursor_key(letter: u8, mods: KeyModifiers, modes: &Modes) -> Vec<u8> {
    if mods.is_empty() {
        let intro = if modes.app_cursor { b'O' } else { b'[' };
        vec![0x1b, intro, letter]
    } else {
        modified(letter, mods)
    }
}

/// F1 to F4: `SS3 x`, or `CSI 1;mods x` when modified.
fn ss3_key(letter: u8, mods: KeyModifiers) -> Vec<u8> {
    if mods.is_empty() {
        vec![0x1b, b'O', letter]
    } else {
        modified(letter, mods)
    }
}

fn modified(letter: u8, mods: KeyModifiers) -> Vec<u8> {
    let mut out = format!("\x1b[1;{}", param(mods)).into_bytes();
    out.push(letter);
    out
}

/// `CSI n ~`, or `CSI n;mods ~` when modified.
fn tilde_key(n: u8, mods: KeyModifiers) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{}~", param(mods)).into_bytes()
    }
}
