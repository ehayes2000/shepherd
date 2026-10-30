//! What keys and other terminal events mean.
//!
//! Keys go to the selected session, encoded for its input modes, except the prefix (`ctrl-b`). After the prefix,
//! one key is a shepherd command (see [`crate::BINDINGS`]) and keys go back to the session; an unbound key
//! cancels the prefix and is dropped, and the prefix again sends a literal `ctrl-b`. An open popup takes every key.
//!
//! The mouse clicks what [`crate::hit`] finds under it (decision 0014), on the release of a press of the same button
//! that went down on the same control ([`Pointer`] remembers it). A left click presses the control; a right click on a
//! session opens its context menu at the click. Under an open popup, a click outside it dismisses it. The wheel
//! scrolls the sidebar when no popup is open, and the profile picker or the workspace switcher over it. Only a session
//! that tracks the mouse gets mouse events in its pane.
//!
//! One key can mean more than one intent (a command also returns keys to the terminal), so these return a list, to
//! be handled in order.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::{Cell, Client, Focus, Intent, Popup};
use shepherd_core::wire::InputKind;
use shepherd_term::{Modes, MouseTracking};

use crate::encode;
use crate::layout::{Target, hit, layout, pane_size, sidebar_rows};
use crate::popup;

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
        Focus::Terminal => input(key_kind(key), encode::key(key, modes)),
    }
}

/// Whether `key` can answer a dialog in the session: Enter, Esc, Ctrl-C or a printable key (shift is how some are
/// typed). The host can't always tell from the bytes, which in kitty keyboard modes may be `CSI … u` for any key.
fn key_kind(key: &KeyEvent) -> InputKind {
    let mods = key.modifiers;
    let answers = match key.code {
        KeyCode::Enter | KeyCode::Esc => mods.is_empty(),
        KeyCode::Char('c') if mods == KeyModifiers::CONTROL => true,
        KeyCode::Char(_) => (mods - KeyModifiers::SHIFT).is_empty(),
        _ => false,
    };
    if answers {
        InputKind::Answer
    } else {
        InputKind::Keys
    }
}

/// What the mouse is doing between events: the button last pressed and the control it went down on.
#[derive(Debug, Clone, Default)]
pub struct Pointer {
    pressed: Option<(MouseButton, Target)>,
}

/// What a terminal event means. `window` is the whole window, for placing mouse events.
pub fn map_event(
    event: &Event,
    client: &Client,
    modes: &Modes,
    window: Rect,
    pointer: &mut Pointer,
) -> Vec<Intent> {
    let to_terminal = client.popup().is_none() && client.focus() == Focus::Terminal;
    match event {
        Event::Key(key) => map_key(key, client, modes),
        Event::Resize(cols, rows) => vec![
            Intent::Resize(pane_size(*cols, *rows)),
            Intent::SidebarRows(sidebar_rows(Rect::new(0, 0, *cols, *rows))),
        ],
        Event::Paste(text) => match (client.popup(), client.focus()) {
            (Some(Popup::Rename { name, .. }), _) => {
                vec![Intent::EditRename(format!("{name}{}", first_line(text)))]
            }
            (Some(Popup::Workspace { path, .. }), _) => {
                vec![Intent::EditWorkspacePath(format!(
                    "{path}{}",
                    first_line(text)
                ))]
            }
            (Some(_), _) => Vec::new(),
            (None, Focus::Command) => vec![
                Intent::SetFocus(Focus::Terminal),
                Intent::Input(InputKind::Paste, encode::paste(text, modes)),
            ],
            (None, Focus::Terminal) => {
                vec![Intent::Input(InputKind::Paste, encode::paste(text, modes))]
            }
        },
        // The window's focus is reported to shepherd, and forwarded to the session too if it asked and keys go to it.
        Event::FocusGained | Event::FocusLost => {
            let gained = *event == Event::FocusGained;
            let mut intents = if to_terminal {
                input(InputKind::Focus, encode::focus(gained, modes))
            } else {
                Vec::new()
            };
            intents.push(Intent::WindowFocus(gained));
            intents
        }
        Event::Mouse(mouse) => map_mouse(mouse, client, modes, window, pointer),
    }
}

/// A mouse event. In the pane of a session that tracks the mouse (with keys going to it), it goes to the session.
/// Otherwise a press and release of one button on the same control clicks it, and the wheel scrolls the sidebar, or
/// the profile picker or workspace switcher while the pointer is over it. An open popup covers everything under it
/// ([`hit`]).
fn map_mouse(
    mouse: &MouseEvent,
    client: &Client,
    modes: &Modes,
    window: Rect,
    pointer: &mut Pointer,
) -> Vec<Intent> {
    // Every press or release ends the earlier press, whatever it lands on (a tracked pane, nothing), so only a press
    // below can arm the next release.
    let pressed = match mouse.kind {
        MouseEventKind::Down(_) | MouseEventKind::Up(_) => pointer.pressed.take(),
        _ => None,
    };
    let layout = layout(window);
    let target = hit(&layout, client, mouse.column, mouse.row);
    let tracked = modes.mouse.tracking != MouseTracking::Off;
    if target == Some(Target::Terminal) && tracked && client.focus() == Focus::Terminal {
        let (col, row) = (mouse.column - layout.pane.x, mouse.row - layout.pane.y);
        return input(InputKind::Mouse, encode::mouse(mouse, col, row, modes));
    }
    let over_sidebar = client.popup().is_none()
        && layout
            .sidebar
            .contains(Position::new(mouse.column, mouse.row));
    let over_popup = matches!(
        target,
        Some(
            Target::Profile(_)
                | Target::WorkspaceRow(_)
                | Target::Popup
                | Target::Confirm
                | Target::Dismiss
        )
    );
    let scroll_popup = |by| match client.popup() {
        Some(Popup::PickProfile) if over_popup => {
            let shown = popup::picker_rows(client, window);
            vec![Intent::ScrollPicker { by, shown }]
        }
        Some(Popup::Workspace { .. }) if over_popup => {
            let shown = popup::switcher_rows(client, window);
            vec![Intent::ScrollSwitcher { by, shown }]
        }
        _ => Vec::new(),
    };
    match mouse.kind {
        MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
            pointer.pressed = target.map(|target| (button, target));
            Vec::new()
        }
        MouseEventKind::Up(button) => match (target, pressed) {
            (Some(target), Some(pressed)) if pressed == (button, target.clone()) => {
                let at = Cell {
                    col: mouse.column,
                    row: mouse.row,
                };
                match button {
                    MouseButton::Left => click(target, client),
                    _ => right_click(target, at),
                }
            }
            _ => Vec::new(),
        },
        MouseEventKind::ScrollDown if over_sidebar => vec![Intent::ScrollSidebar(1)],
        MouseEventKind::ScrollUp if over_sidebar => vec![Intent::ScrollSidebar(-1)],
        MouseEventKind::ScrollDown => scroll_popup(1),
        MouseEventKind::ScrollUp => scroll_popup(-1),
        _ => Vec::new(),
    }
}

/// A paste's first line: what an input takes of it.
fn first_line(text: &str) -> &str {
    text.split(['\r', '\n']).next().unwrap_or_default()
}

/// A left click on `target`.
fn click(target: Target, client: &Client) -> Vec<Intent> {
    match target {
        Target::Workspace => vec![Intent::BeginWorkspace],
        Target::WorkspaceRow(path) => vec![Intent::PickWorkspace(path)],
        Target::NewSession => vec![Intent::BeginNewSession],
        Target::Session(id) if client.selected() == Some(&id) => {
            vec![Intent::SetFocus(Focus::Terminal)]
        }
        Target::Session(id) => vec![Intent::SelectSession(id), Intent::SetFocus(Focus::Terminal)],
        Target::CloseSession(id) => vec![Intent::BeginCloseSession(id)],
        Target::Terminal => vec![Intent::SetFocus(Focus::Terminal)],
        Target::Detach => vec![Intent::Quit],
        Target::Help => vec![Intent::ShowHelp],
        Target::MenuItem(item) => vec![Intent::MenuChoose(item)],
        Target::Confirm => vec![Intent::Confirm],
        Target::Dismiss | Target::Backdrop => vec![Intent::Dismiss],
        Target::Profile(name) => vec![Intent::PickProfile(name)],
        Target::Popup => Vec::new(),
    }
}

/// A right click on `target`, at `at`: a session's context menu, or away from an open popup.
fn right_click(target: Target, at: Cell) -> Vec<Intent> {
    match target {
        Target::Session(session) | Target::CloseSession(session) => {
            vec![Intent::OpenMenu { session, at }]
        }
        Target::Backdrop => vec![Intent::Dismiss],
        _ => Vec::new(),
    }
}

/// Input for the session, tagged with what made it, so the host knows a mouse or focus report isn't the user
/// answering a dialog, and which keys can be.
fn input(kind: InputKind, bytes: Option<Vec<u8>>) -> Vec<Intent> {
    bytes
        .map(|bytes| Intent::Input(kind, bytes))
        .into_iter()
        .collect()
}

/// The key after the prefix.
fn command(key: &KeyEvent, modes: &Modes) -> Option<Intent> {
    if is_prefix(key) {
        return encode::key(key, modes).map(Intent::keys);
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
        Popup::Rename { name, .. } => input_key(key, name, plain, Intent::EditRename),
        Popup::Workspace { path, .. } => input_key(key, path, plain, Intent::EditWorkspacePath),
        Popup::ConfirmClose { .. } => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter if plain => Some(Intent::Confirm),
            KeyCode::Char('n' | 'N') | KeyCode::Esc if plain => Some(Intent::Dismiss),
            _ => None,
        },
        Popup::Menu { .. } | Popup::PickProfile => {
            (key.code == KeyCode::Esc).then_some(Intent::Dismiss)
        }
    }
}

/// A key in a popup with a text input holding `text`: enter accepts, esc dismisses, and editing keys give `edit` of the
/// new text.
fn input_key(
    key: &KeyEvent,
    text: &str,
    plain: bool,
    edit: fn(String) -> Intent,
) -> Option<Intent> {
    match key.code {
        KeyCode::Enter => Some(Intent::Confirm),
        KeyCode::Esc => Some(Intent::Dismiss),
        KeyCode::Backspace => {
            let mut text = text.to_owned();
            text.pop();
            Some(edit(text))
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => Some(edit(String::new())),
        KeyCode::Char(c) if plain => Some(edit(format!("{text}{c}"))),
        _ => None,
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
    // Other modifiers stay: a legacy terminal sends alt as an ESC prefix, which crossterm reports as ctrl+alt+digit.
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
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
