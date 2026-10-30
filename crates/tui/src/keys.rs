//! What keys and other terminal events mean.
//!
//! Keys go to the selected session, encoded for its input modes, except the prefix (`ctrl-b`). After the prefix,
//! one key is a shepherd command (see [`crate::help::BINDINGS`]) and keys go back to the session; an unbound key
//! cancels the prefix and is dropped, and the prefix again sends a literal `ctrl-b`. An open popup takes every key.
//!
//! One key can mean more than one intent (a command also returns keys to the terminal), so these return a list, to
//! be handled in order.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::{Client, Focus, Intent, Popup};
use shepherd_term::Modes;

use crate::encode;
use crate::layout::{layout, pane_size};

/// The prefix key. Configurable later.
pub const PREFIX: char = 'b';

fn is_prefix(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char(PREFIX) && key.modifiers == KeyModifiers::CONTROL
}

/// What `key` means, given the client's focus and popup and the selected session's input `modes`.
pub fn map_key(key: &KeyEvent, client: &Client, modes: &Modes) -> Vec<Intent> {
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    if let Some(popup) = client.popup() {
        return popup_key(key, popup).into_iter().collect();
    }
    match client.focus() {
        Focus::Command => {
            let mut intents = vec![Intent::SetFocus(Focus::Terminal)];
            intents.extend(command(key, modes));
            intents
        }
        Focus::Terminal if is_prefix(key) => vec![Intent::SetFocus(Focus::Command)],
        Focus::Terminal => input(encode::key(key, modes)),
    }
}

/// What a terminal event means. `window` is the whole window, for placing mouse events.
pub fn map_event(event: &Event, client: &Client, modes: &Modes, window: Rect) -> Vec<Intent> {
    let to_terminal = client.popup().is_none() && client.focus() == Focus::Terminal;
    match event {
        Event::Key(key) => map_key(key, client, modes),
        Event::Resize(cols, rows) => vec![Intent::Resize(pane_size(*cols, *rows))],
        Event::Paste(text) => match (client.popup(), client.focus()) {
            (Some(Popup::Rename { name, .. }), _) => {
                let line = text.split(['\r', '\n']).next().unwrap_or_default();
                vec![Intent::EditRename(format!("{name}{line}"))]
            }
            (Some(_), _) => Vec::new(),
            (None, Focus::Command) => vec![
                Intent::SetFocus(Focus::Terminal),
                Intent::Input(encode::paste(text, modes)),
            ],
            (None, Focus::Terminal) => vec![Intent::Input(encode::paste(text, modes))],
        },
        Event::FocusGained | Event::FocusLost if to_terminal => {
            input(encode::focus(*event == Event::FocusGained, modes))
        }
        Event::Mouse(mouse) if to_terminal => {
            let pane = layout(window).pane;
            if !pane.contains(Position::new(mouse.column, mouse.row)) {
                return Vec::new();
            }
            let (col, row) = (mouse.column - pane.x, mouse.row - pane.y);
            input(encode::mouse(mouse, col, row, modes))
        }
        Event::FocusGained | Event::FocusLost | Event::Mouse(_) => Vec::new(),
    }
}

fn input(bytes: Option<Vec<u8>>) -> Vec<Intent> {
    bytes.map(Intent::Input).into_iter().collect()
}

/// The key after the prefix.
fn command(key: &KeyEvent, modes: &Modes) -> Option<Intent> {
    if is_prefix(key) {
        return encode::key(key, modes).map(Intent::Input);
    }
    // Shift is how some keys (`?`) are typed at all.
    if !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('n') => Intent::NewSession,
        KeyCode::Char('j') | KeyCode::Down => Intent::NextSession,
        KeyCode::Char('k') | KeyCode::Up => Intent::PrevSession,
        KeyCode::Char('r') => Intent::BeginRename,
        KeyCode::Char('x') => Intent::BeginClose,
        KeyCode::Char('?') => Intent::ShowHelp,
        KeyCode::Char('q') => Intent::Quit,
        _ => return None,
    })
}

fn popup_key(key: &KeyEvent, popup: &Popup) -> Option<Intent> {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    match popup {
        Popup::Help => Some(Intent::Dismiss),
        Popup::Rename { name, .. } => match key.code {
            KeyCode::Enter => Some(Intent::Confirm),
            KeyCode::Esc => Some(Intent::Dismiss),
            KeyCode::Backspace => {
                let mut name = name.clone();
                name.pop();
                Some(Intent::EditRename(name))
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                Some(Intent::EditRename(String::new()))
            }
            KeyCode::Char(c) if plain => Some(Intent::EditRename(format!("{name}{c}"))),
            _ => None,
        },
        Popup::ConfirmClose { .. } => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter if plain => Some(Intent::Confirm),
            KeyCode::Char('n' | 'N') | KeyCode::Esc if plain => Some(Intent::Dismiss),
            _ => None,
        },
    }
}

/// Undoes how crossterm names control bytes 0x1C to 0x1F from a terminal that doesn't report keys unambiguously: it
/// reports them as ctrl plus `4` to `7` (`event/sys/unix/parse.rs`). They become the keys usually pressed for them,
/// `ctrl-\`, `ctrl-]`, `ctrl-^` and `ctrl-/`, so a session in kitty keyboard mode gets those keys' codes. Use it only
/// when the outer terminal isn't reporting keys unambiguously; there, ctrl plus a digit is really that.
pub fn from_legacy_terminal(key: KeyEvent) -> KeyEvent {
    let KeyCode::Char(c @ '4'..='7') = key.code else {
        return key;
    };
    if key.modifiers != KeyModifiers::CONTROL {
        return key;
    }
    let pressed = match c {
        '4' => '\\',
        '5' => ']',
        '6' => '^',
        _ => '/',
    };
    KeyEvent {
        code: KeyCode::Char(pressed),
        ..key
    }
}
