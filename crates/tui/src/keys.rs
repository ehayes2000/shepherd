//! What keys and other terminal events mean.
//!
//! Keys go to the selected session, encoded for its input modes, except the prefix (`ctrl-b`). After the prefix,
//! one key is a shepherd command (see [`crate::BINDINGS`]) and keys go back to the session; an unbound key
//! cancels the prefix and is dropped, and the prefix again sends a literal `ctrl-b`. An open popup takes every key.
//!
//! The mouse clicks what [`crate::hit`] finds under it (decision 0014), on the release of a press of the same button
//! that went down on the same control ([`Pointer`] remembers it). A left click presses the control; a right click on a
//! session opens its context menu at the click, and in a pane, the pane's menu. Under an open popup, a click outside it
//! dismisses it. A left press on a divider between split panes drags it: its release, wherever it lands, sets the
//! split's ratio (one layout op per drag). The wheel
//! scrolls the sidebar when no popup is open, and the profile picker, the workspace switcher, the finder or the cleanup
//! view over it. Only a session
//! that tracks the mouse gets mouse events, in the focused pane's terminal; a click in another pane focuses it.
//!
//! One key can mean more than one intent (a command also returns keys to the terminal), so these return a list, to
//! be handled in order.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use shepherd_core::client::panes::Divider;
use shepherd_core::client::{Cell, Client, Focus, Intent, Popup};
use shepherd_core::layout::{Content, LayoutOp};
use shepherd_core::wire::InputKind;
use shepherd_term::{Modes, MouseTracking};

use crate::encode;
use crate::layout::{Target, hit, layout, pane_size, sidebar_rows};
use crate::{cleanup, finder, panes, popup, status};

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
            intents.extend(command(key, client, modes));
            intents
        }
        Focus::Terminal if is_prefix(key) => vec![Intent::SetFocus(Focus::Command)],
        Focus::Terminal if status::focused(client) => status_key(key, client).into_iter().collect(),
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

/// What the mouse is doing between events: the button last pressed and the control it went down on, and a divider
/// being dragged with the cell it was grabbed at.
#[derive(Debug, Clone, Default)]
pub struct Pointer {
    pressed: Option<(MouseButton, Target)>,
    dragging: Option<(Divider, (u16, u16))>,
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
            (Some(Popup::Finder { query }), _) => {
                vec![Intent::EditFind(format!("{query}{}", first_line(text)))]
            }
            (Some(_), _) => Vec::new(),
            (None, Focus::Terminal) if status::focused(client) => match client.composing() {
                true => vec![Intent::EditMessage(format!(
                    "{}{}",
                    client.message_text(),
                    first_line(text)
                ))],
                false => Vec::new(),
            },
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
    let (pressed, dragging) = match mouse.kind {
        MouseEventKind::Down(_) | MouseEventKind::Up(_) => {
            (pointer.pressed.take(), pointer.dragging.take())
        }
        _ => (None, None),
    };
    if let (MouseEventKind::Up(MouseButton::Left), Some((divider, from))) = (mouse.kind, dragging) {
        return divider
            .dragged(from, (mouse.column, mouse.row))
            .map(|ratio| {
                Intent::ApplyLayout(LayoutOp::Ratio {
                    pane: divider.split,
                    ratio,
                })
            })
            .into_iter()
            .collect();
    }
    let layout = layout(window);
    let target = hit(&layout, client, mouse.column, mouse.row);
    let tracked = modes.mouse.tracking != MouseTracking::Off;
    if target == Some(Target::Terminal) && tracked && client.focus() == Focus::Terminal {
        let origin = panes::focused_terminal(client, layout.pane).unwrap_or(layout.pane);
        let col = mouse.column.saturating_sub(origin.x);
        let row = mouse.row.saturating_sub(origin.y);
        return input(InputKind::Mouse, encode::mouse(mouse, col, row, modes));
    }
    let over_sidebar = client.popup().is_none()
        && layout
            .sidebar
            .contains(Position::new(mouse.column, mouse.row));
    let over_status = client
        .popup()
        .is_none()
        .then(|| status::under(client, layout.pane, mouse.column, mouse.row))
        .flatten();
    let over_popup = matches!(
        target,
        Some(
            Target::Profile(_)
                | Target::WorkspaceRow(_)
                | Target::MachineRow(_)
                | Target::FinderRow(_)
                | Target::Popup
                | Target::Confirm
                | Target::Dismiss
                | Target::RemoveWorktree
                | Target::KeepWorktree
                | Target::RestartHost
                | Target::CleanupRemoveSafe
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
        Some(Popup::Finder { .. }) if over_popup => {
            let shown = finder::rows(client, window);
            vec![Intent::ScrollFinder { by, shown }]
        }
        Some(Popup::Cleanup) if over_popup => {
            let shown = cleanup::rows(client, window);
            vec![Intent::ScrollCleanup { by, shown }]
        }
        _ => Vec::new(),
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if matches!(target, Some(Target::Divider(_))) => {
            if let Some(Target::Divider(divider)) = target {
                pointer.dragging = Some((divider, (mouse.column, mouse.row)));
            }
            Vec::new()
        }
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
                    MouseButton::Left => click(target, client, at),
                    _ => right_click(target, client, at),
                }
            }
            _ => Vec::new(),
        },
        MouseEventKind::ScrollDown if over_sidebar => vec![Intent::ScrollSidebar(1)],
        MouseEventKind::ScrollUp if over_sidebar => vec![Intent::ScrollSidebar(-1)],
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if over_status.is_some() => {
            let by = if mouse.kind == MouseEventKind::ScrollDown {
                1
            } else {
                -1
            };
            let shown = over_status.unwrap_or_default();
            vec![Intent::StatusScroll { by, shown }]
        }
        MouseEventKind::ScrollDown => scroll_popup(1),
        MouseEventKind::ScrollUp => scroll_popup(-1),
        _ => Vec::new(),
    }
}

/// A paste's first line: what an input takes of it.
fn first_line(text: &str) -> &str {
    text.split(['\r', '\n']).next().unwrap_or_default()
}

/// A left click on `target`, at `at`.
fn click(target: Target, client: &Client, at: Cell) -> Vec<Intent> {
    match target {
        Target::Search => vec![Intent::BeginFind],
        Target::FinderRow(item) => vec![Intent::FinderChoose(item)],
        Target::Workspace => vec![Intent::BeginWorkspace],
        Target::WorkspaceRow(path) => vec![Intent::PickWorkspace(path)],
        Target::NewSession => vec![Intent::BeginNewSession],
        Target::Session(id) if client.selected() == Some(&id) => {
            vec![Intent::SetFocus(Focus::Terminal)]
        }
        Target::Session(id) => vec![Intent::SelectSession(id), Intent::SetFocus(Focus::Terminal)],
        Target::CloseSession(id) => vec![Intent::BeginCloseSession(id)],
        Target::Subagents(id) => vec![Intent::ToggleSubagents(id)],
        Target::Subagent { session, .. } if client.selected() == Some(&session) => {
            vec![Intent::SetFocus(Focus::Terminal)]
        }
        Target::Subagent { session, .. } => vec![
            Intent::SelectSession(session),
            Intent::SetFocus(Focus::Terminal),
        ],
        Target::Machine(machine) => vec![Intent::SelectMachine(machine)],
        Target::Terminal => vec![Intent::SetFocus(Focus::Terminal)],
        Target::Detach => vec![Intent::Quit],
        Target::Help => vec![Intent::ShowHelp],
        Target::MenuItem(item) => vec![Intent::MenuChoose(item)],
        Target::Confirm => vec![Intent::Confirm],
        Target::RemoveWorktree => vec![Intent::CloseRemovingWorktree],
        Target::KeepWorktree => vec![Intent::CloseKeepingWorktree],
        Target::RestartHost => vec![Intent::RestartHost],
        Target::OfferRestart => vec![Intent::OfferRestart],
        Target::ShowHostWarning => vec![Intent::ShowHostWarning],
        Target::Cleanup => vec![Intent::BeginCleanup],
        Target::CleanupRemoveSafe => vec![Intent::CleanupRemoveSafe],
        Target::Dismiss | Target::Backdrop => vec![Intent::Dismiss],
        Target::Profile(name) => vec![Intent::PickProfile(name)],
        Target::MachineRow(machine) => vec![Intent::PickMachine(machine)],
        Target::Pane(pane) => vec![Intent::FocusPane(pane)],
        Target::PaneMenu(pane) => vec![Intent::OpenPaneMenu { pane, at }],
        Target::LayoutMenu => vec![Intent::OpenPaneMenu {
            pane: client.focused_pane(),
            at,
        }],
        Target::PaneMenuItem(item) => vec![Intent::PaneMenuChoose(item)],
        Target::Page(page) if page == client.host_layout().current() => Vec::new(),
        Target::Page(page) => vec![Intent::ApplyLayout(LayoutOp::PageSelect { page })],
        Target::NewPage => vec![Intent::ApplyLayout(LayoutOp::PageNew {
            name: None,
            content: Content::Selected,
        })],
        // Dragged, not clicked.
        Target::Divider(_) => Vec::new(),
        Target::Popup => Vec::new(),
        // A click on the focused pane's selected row attaches it; any other selects it, focusing its pane.
        Target::StatusRow {
            pane,
            session,
            shown,
        } => {
            if pane == client.focused_pane() && client.status_selected() == Some(&session) {
                return vec![Intent::StatusAttach(session)];
            }
            vec![
                Intent::FocusPane(pane),
                Intent::StatusSelect { session, shown },
            ]
        }
        Target::StatusGroupBy { pane, by, shown } => {
            vec![Intent::FocusPane(pane), Intent::StatusGroupBy { by, shown }]
        }
        Target::StatusEvent {
            pane,
            session,
            shown,
        } => vec![
            Intent::FocusPane(pane),
            Intent::StatusSelect { session, shown },
        ],
        Target::StatusTarget { pane, next } => {
            vec![Intent::FocusPane(pane), Intent::MessageTarget(next)]
        }
        Target::StatusMessage { pane } => {
            vec![Intent::FocusPane(pane), Intent::ComposeMessage(true)]
        }
        Target::StatusSend { pane } => vec![Intent::FocusPane(pane), Intent::SendMessage],
    }
}

/// A right click on `target`, at `at`: a session's context menu, a pane's menu, or away from an open popup.
fn right_click(target: Target, client: &Client, at: Cell) -> Vec<Intent> {
    match target {
        Target::Session(session) | Target::CloseSession(session) => {
            vec![Intent::OpenMenu { session, at }]
        }
        Target::Terminal => vec![Intent::OpenPaneMenu {
            pane: client.focused_pane(),
            at,
        }],
        Target::Pane(pane) | Target::PaneMenu(pane) => vec![Intent::OpenPaneMenu { pane, at }],
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
fn command(key: &KeyEvent, client: &Client, modes: &Modes) -> Option<Intent> {
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
        KeyCode::Tab => Intent::ToggleSubagents(client.selected()?.clone()),
        KeyCode::Char('s') => Intent::OpenStatus,
        _ => return None,
    })
}

/// A key while the focused pane shows the status page: `tab` widens the message line's target. While the message line
/// takes the keys (it was clicked), keys edit it, `⏎` sends it and `esc` gives the keys back to the table. Otherwise
/// `g` switches the grouping, `↑`/`↓` (or `k`/`j`) and page up and down move the table's selection, and `⏎` attaches
/// it. Other keys do nothing: there is no session to type into.
fn status_key(key: &KeyEvent, client: &Client) -> Option<Intent> {
    if key.code == KeyCode::Tab && key.modifiers.is_empty() {
        return Some(Intent::MessageTarget(client.next_message_target()));
    }
    if client.composing() {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        return match key.code {
            KeyCode::Enter => Some(Intent::SendMessage),
            KeyCode::Esc => Some(Intent::ComposeMessage(false)),
            _ => edit_key(key, client.message_text(), plain, Intent::EditMessage),
        };
    }
    if !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    let shown = status::focused_shown(client);
    let page = i32::try_from(shown.saturating_sub(1).max(1)).unwrap_or(i32::MAX);
    let step = |by| Some(Intent::StatusStep { by, shown });
    match key.code {
        KeyCode::Char('g') => Some(Intent::StatusGroupBy {
            by: client.status_grouping().other(),
            shown,
        }),
        KeyCode::Down | KeyCode::Char('j') => step(1),
        KeyCode::Up | KeyCode::Char('k') => step(-1),
        KeyCode::PageDown => step(page),
        KeyCode::PageUp => step(-page),
        KeyCode::Enter => Some(Intent::StatusAttach(client.status_selected()?.clone())),
        _ => None,
    }
}

fn popup_key(key: &KeyEvent, popup: &Popup) -> Option<Intent> {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    match popup {
        Popup::Help => Some(Intent::Dismiss),
        Popup::Rename { name, .. } => input_key(key, name, plain, Intent::EditRename),
        Popup::Workspace { path, .. } => input_key(key, path, plain, Intent::EditWorkspacePath),
        Popup::Finder { query } => input_key(key, query, plain, Intent::EditFind),
        Popup::ConfirmClose { .. } | Popup::StartInWorktree { .. } => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter if plain => Some(Intent::Confirm),
            KeyCode::Char('n' | 'N') | KeyCode::Esc if plain => Some(Intent::Dismiss),
            _ => None,
        },
        // Only a click removes anything.
        Popup::Menu { .. } | Popup::PaneMenu { .. } | Popup::PickProfile | Popup::Cleanup => {
            (key.code == KeyCode::Esc).then_some(Intent::Dismiss)
        }
        // Only a click restarts the host.
        Popup::RestartHost { .. } => match key.code {
            KeyCode::Char('n' | 'N') | KeyCode::Esc if plain => Some(Intent::Dismiss),
            _ => None,
        },
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
        _ => edit_key(key, text, plain, edit),
    }
}

/// An editing key in a text input holding `text`: backspace, `ctrl-u` to clear, or a character typed.
fn edit_key(key: &KeyEvent, text: &str, plain: bool, edit: fn(String) -> Intent) -> Option<Intent> {
    match key.code {
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
