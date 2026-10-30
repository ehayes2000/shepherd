//! Popups drawn over everything: help, the rename input, the close confirmation, a session's context menu, the
//! profile picker for a new session, and the workspace switcher.
//!
//! Dialogs are centered, with their buttons right-aligned on their last line. The menu opens with its top-left corner
//! where it was asked for, moved in to fit the window, wide enough for its longest line. An item that can't be chosen
//! is drawn dim with the reason, and isn't a control. [`place`] says where a popup and its controls are, for both
//! drawing and [`crate::hit`].

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use shepherd_core::client::{Cell, Client, MenuEntry, MenuItem, Popup};
use shepherd_core::wire::ProfileInfo;

use crate::layout::Target;

/// The prefix bindings, as the help lists them.
pub const BINDINGS: [(&str, &str); 8] = [
    ("ctrl-b n", "new session"),
    ("ctrl-b j / ↓", "next session"),
    ("ctrl-b k / ↑", "previous session"),
    ("ctrl-b r", "rename session"),
    ("ctrl-b x", "close session"),
    ("ctrl-b ?", "this help"),
    ("ctrl-b q", "detach (agents keep running)"),
    ("ctrl-b ctrl-b", "send ctrl-b to the session"),
];

/// How to use shepherd with the mouse, which the help says first.
const MOUSE: [&str; 6] = [
    "Click + New session to start a session, a session to",
    "switch to it, and the terminal to type in it.",
    "Right-click a session to rename or close it, or click",
    "its × to close it. Detach leaves the agents running.",
    "Select text with shift-drag (option-drag in iTerm2",
    "and Terminal.app).",
];

/// The menu's narrowest width, its border included.
const MENU_WIDTH: u16 = 10;
/// A dialog's width, its border included.
const DIALOG_WIDTH: u16 = 40;
/// The rename and close dialogs' height: two lines and the buttons, in a border.
const DIALOG_HEIGHT: u16 = 5;
const HELP_WIDTH: u16 = 56;

/// The profile picker's line above its rows.
const PICK_PROMPT: &str = "Which agent should the session run?";
/// How the default profile's row is marked.
const DEFAULT_MARK: &str = "(default)";

/// The workspace switcher's lines: above its rows of workspaces used before, then above its path input.
const USED_BEFORE: &str = "Used before:";
const OPEN_PROMPT: &str = "Open a repo by path:";
const OPEN_HINT: &str = "enter to open · esc to cancel";
/// How the current workspace's row is marked.
const CURRENT_MARK: &str = "(current)";
const SWITCHER_WIDTH: u16 = 56;
/// The switcher's lines below its rows: the prompt, the input, the error or hint, and the buttons.
const SWITCHER_TAIL: u16 = 4;

/// A clickable part of a popup.
pub(crate) struct Control {
    pub area: Rect,
    pub target: Target,
    pub label: String,
}

/// Where the open popup is in the window.
pub(crate) struct Placed {
    /// Its box, border included.
    pub area: Rect,
    pub controls: Vec<Control>,
    /// Lines drawn dim that aren't controls: a menu's items that can't be chosen.
    pub disabled: Vec<(Position, String)>,
}

/// Where the open popup, if any, and its controls are in `window`.
pub(crate) fn place(client: &Client, window: Rect) -> Option<Placed> {
    Some(match client.popup()? {
        Popup::Help => {
            let height = help_lines().len() as u16 + 3;
            dialog(
                centered(window, HELP_WIDTH, height),
                &[("[Close]", Target::Dismiss)],
            )
        }
        Popup::Rename { .. } => dialog(
            centered(window, DIALOG_WIDTH, DIALOG_HEIGHT),
            &[("[Save]", Target::Confirm), ("[Cancel]", Target::Dismiss)],
        ),
        Popup::ConfirmClose { .. } => dialog(
            centered(window, DIALOG_WIDTH, DIALOG_HEIGHT),
            &[("[Close]", Target::Confirm), ("[Cancel]", Target::Dismiss)],
        ),
        Popup::Menu { at, .. } => menu(&client.menu(), *at, window),
        Popup::PickProfile => pick_profile(client, window),
        Popup::Workspace { .. } => switcher(client, window),
    })
}

/// A dialog in `area` with `buttons` right-aligned on its last line, a column in from the border.
fn dialog(area: Rect, buttons: &[(&'static str, Target)]) -> Placed {
    let inside = inner(area);
    let mut controls = Vec::new();
    if inside.height > 0 {
        let y = inside.bottom() - 1;
        let mut end = inside.right().saturating_sub(1);
        for (label, target) in buttons.iter().rev() {
            let width = label.chars().count() as u16;
            let Some(x) = end.checked_sub(width).filter(|&x| x >= inside.x) else {
                break;
            };
            controls.push(Control {
                area: Rect::new(x, y, width, 1),
                target: target.clone(),
                label: (*label).to_owned(),
            });
            end = x.saturating_sub(2);
        }
        controls.reverse();
    }
    Placed {
        area,
        controls,
        disabled: Vec::new(),
    }
}

/// How a menu entry is drawn: its name, and why it can't be chosen if it can't.
fn menu_label(entry: &MenuEntry) -> String {
    let name = match entry.item {
        MenuItem::Rename => "Rename",
        MenuItem::Close => "Close",
        MenuItem::Resume => "Resume",
    };
    match entry.unavailable {
        None => format!(" {name}"),
        Some(why) => format!(" {name} ({why})"),
    }
}

/// The menu, its top-left corner at `at` or as near as fits in `window`. Each item is a whole line of it.
fn menu(entries: &[MenuEntry], at: Cell, window: Rect) -> Placed {
    let labels: Vec<String> = entries.iter().map(menu_label).collect();
    // A column after the longest label, and the border.
    let longest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let wanted = u16::try_from(longest + 3).unwrap_or(u16::MAX);
    let width = wanted.max(MENU_WIDTH).min(window.width);
    let height = (entries.len() as u16 + 2).min(window.height);
    let x = at.col.min(window.right() - width).max(window.x);
    let y = at.row.min(window.bottom() - height).max(window.y);
    let area = Rect::new(x, y, width, height);
    let inside = inner(area);
    let mut placed = Placed {
        area,
        controls: Vec::new(),
        disabled: Vec::new(),
    };
    for ((entry, label), y) in entries.iter().zip(labels).zip(inside.y..inside.bottom()) {
        if entry.unavailable.is_some() {
            placed.disabled.push((Position::new(inside.x, y), label));
        } else {
            placed.controls.push(Control {
                area: Rect::new(inside.x, y, inside.width, 1),
                target: Target::MenuItem(entry.item),
                label,
            });
        }
    }
    placed
}

/// The profile picker: a prompt, then one row per profile (the default first and marked), each a whole line of the
/// dialog, then `[Cancel]`. It is as tall as its profiles and fits the window; the rows shown start at the client's
/// scroll, moved up as far as the last rows fill it. Its columns are as wide as their longest entries, but a long
/// name is cut short with `…` so the agent and the mark stay visible.
fn pick_profile(client: &Client, window: Rect) -> Placed {
    let profiles = client.profiles();
    let count = u16::try_from(profiles.len()).unwrap_or(u16::MAX);
    let mut placed = dialog(
        centered(window, DIALOG_WIDTH, count.saturating_add(4)),
        &[("[Cancel]", Target::Dismiss)],
    );
    let inside = inner(placed.area);
    let widest = |width: fn(&ProfileInfo) -> usize| profiles.iter().map(width).max().unwrap_or(0);
    let agent_width = widest(|p| p.agent.as_str().chars().count());
    let mark_width = match client.default_profile() {
        Some(_) => DEFAULT_MARK.chars().count() + 2,
        None => 0,
    };
    // The leading blank, and two blanks after the name.
    let room = usize::from(inside.width).saturating_sub(3 + agent_width + mark_width);
    let name_width = widest(|p| p.name.chars().count()).min(room.max(1));
    let rows = (inside.y + 1)..inside.bottom().saturating_sub(1);
    let first = client
        .picker_scroll()
        .min(profiles.len().saturating_sub(rows.len()));
    for (profile, y) in profiles[first..].iter().zip(rows) {
        let mark = if client.default_profile() == Some(profile.name.as_str()) {
            DEFAULT_MARK
        } else {
            ""
        };
        let label = format!(
            " {:<name_width$}  {:<agent_width$}  {mark}",
            cut(&profile.name, name_width),
            profile.agent.as_str()
        );
        placed.controls.push(Control {
            area: Rect::new(inside.x, y, inside.width, 1),
            target: Target::Profile(profile.name.clone()),
            label: label.trim_end().to_owned(),
        });
    }
    placed
}

/// How many profiles the open picker shows at once in `window`.
pub(crate) fn picker_rows(client: &Client, window: Rect) -> usize {
    let placed = pick_profile(client, window);
    // Its border, the prompt and the buttons.
    usize::from(placed.area.height.saturating_sub(4))
}

/// `text` cut to `width` characters, its last one `…` if any were cut.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The workspace switcher: the workspaces used before under a line saying so, each a whole line of the dialog with
/// the current one marked; then the path prompt, the input, its error or a hint, and `[Open]` `[Cancel]`.
fn switcher(client: &Client, window: Rect) -> Placed {
    let used = client.workspaces();
    let list = if used.is_empty() {
        0
    } else {
        used.len() as u16 + 1
    };
    let height = list + SWITCHER_TAIL + 2;
    let mut placed = dialog(
        centered(window, SWITCHER_WIDTH, height),
        &[("[Open]", Target::Confirm), ("[Cancel]", Target::Dismiss)],
    );
    let inside = inner(placed.area);
    let rows = (inside.y + 1)..inside.bottom().saturating_sub(SWITCHER_TAIL);
    for (path, y) in used.iter().zip(rows) {
        let mark = if client.workspace() == Some(path.as_path()) {
            CURRENT_MARK
        } else {
            ""
        };
        let label = format!(" {}  {mark}", path.display());
        placed.controls.push(Control {
            area: Rect::new(inside.x, y, inside.width, 1),
            target: Target::WorkspaceRow(path.clone()),
            label: label.trim_end().to_owned(),
        });
    }
    placed
}

/// Draws the switcher's text in `body`, the dialog above its buttons: the line above the rows, the prompt, the typed
/// `path`, and `error` or a hint. Returns where the cursor goes.
fn switcher_body(
    client: &Client,
    path: &str,
    error: Option<&str>,
    body: Rect,
    buf: &mut Buffer,
) -> Option<Position> {
    if body.width < 2 || body.height < SWITCHER_TAIL - 1 {
        return None;
    }
    let width = usize::from(body.width);
    let gray = Style::new().fg(Color::Gray);
    if !client.workspaces().is_empty() && body.height > SWITCHER_TAIL - 1 {
        buf.set_stringn(body.x, body.y, USED_BEFORE, width, gray);
    }
    let bottom = body.bottom();
    buf.set_stringn(body.x, bottom - 3, OPEN_PROMPT, width, gray);
    let cursor = text_input(path, Rect::new(body.x, bottom - 2, body.width, 1), buf);
    match error {
        Some(error) => buf.set_stringn(
            body.x,
            bottom - 1,
            error,
            width,
            Style::new().fg(Color::Red),
        ),
        None => buf.set_stringn(
            body.x,
            bottom - 1,
            OPEN_HINT,
            width,
            gray.add_modifier(Modifier::DIM),
        ),
    };
    cursor
}

/// Draws the open popup, if any, over `area`. Returns where the cursor goes, for a popup with an input.
pub(crate) fn render(client: &Client, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let popup = client.popup()?;
    let placed = place(client, area)?;
    let title = match popup {
        Popup::Help => " help ",
        Popup::Rename { .. } => " rename session ",
        Popup::ConfirmClose { .. } => " close session ",
        Popup::Menu { .. } => "",
        Popup::PickProfile => " new session ",
        Popup::Workspace { .. } => " workspace ",
    };
    frame(placed.area, title, buf);
    let inside = inner(placed.area);
    // Above a dialog's buttons.
    let body = Rect {
        height: inside.height.saturating_sub(1),
        ..inside
    };
    let cursor = match popup {
        Popup::Help => {
            Paragraph::new(help_lines()).render(body, buf);
            None
        }
        Popup::Rename { name, .. } => rename(name, body, buf),
        Popup::ConfirmClose { session } => {
            let name = client
                .sessions()
                .iter()
                .find(|s| &s.id == session)
                .map_or(session.as_str(), |s| s.name.as_str());
            confirm_close(name, body, buf);
            None
        }
        Popup::PickProfile => {
            let style = Style::new().fg(Color::Gray);
            buf.set_stringn(body.x, body.y, PICK_PROMPT, usize::from(body.width), style);
            None
        }
        Popup::Menu { .. } => None,
        Popup::Workspace { path, error } => {
            switcher_body(client, path, error.as_deref(), body, buf)
        }
    };
    let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    for control in &placed.controls {
        let style = match (popup, &control.target) {
            (Popup::Menu { .. }, _) | (_, Target::Profile(_) | Target::WorkspaceRow(_)) => {
                Style::new()
            }
            _ => button,
        };
        let at = control.area;
        buf.set_stringn(at.x, at.y, &control.label, usize::from(at.width), style);
    }
    let dim = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    for (at, label) in &placed.disabled {
        let room = usize::from(inside.right().saturating_sub(at.x));
        buf.set_stringn(at.x, at.y, label, room, dim);
    }
    cursor
}

fn help_lines() -> Vec<Line<'static>> {
    let key = Style::new().fg(Color::Cyan);
    let mut lines: Vec<Line> = MOUSE.iter().map(|l| Line::raw(*l)).collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Or use the keys:",
        Style::new().fg(Color::Gray),
    ));
    lines.extend(BINDINGS.iter().map(|(keys, what)| {
        Line::from(vec![
            Span::styled(format!("{keys:<14}"), key),
            Span::raw(*what),
        ])
    }));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Other keys go to the session. Any key closes this.",
        Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
    ));
    lines
}

/// A one-line text input showing `text` in `line`, keeping its end in view with a column for the cursor. Returns
/// where the cursor goes.
fn text_input(text: &str, line: Rect, buf: &mut Buffer) -> Option<Position> {
    if line.width < 2 {
        return None;
    }
    let room = usize::from(line.width - 1);
    let chars: Vec<char> = text.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    let (end, _) = buf.set_stringn(line.x, line.y, &shown, room, Style::new());
    Some(Position::new(end, line.y))
}

/// The input and its hint, in `body`.
fn rename(name: &str, body: Rect, buf: &mut Buffer) -> Option<Position> {
    if body.width < 2 || body.height < 2 {
        return None;
    }
    let cursor = text_input(name, Rect { height: 1, ..body }, buf);
    let hint = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    buf.set_stringn(
        body.x,
        body.y + 1,
        "enter to rename · esc to cancel",
        usize::from(body.width),
        hint,
    );
    cursor
}

fn confirm_close(name: &str, body: Rect, buf: &mut Buffer) {
    let lines = vec![
        Line::from(vec![
            Span::raw("Close "),
            Span::styled(name, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("? Its agent is stopped."),
        ]),
        Line::styled(
            "y / enter to close · n / esc to keep",
            Style::new().fg(Color::Gray),
        ),
    ];
    Paragraph::new(lines).render(body, buf);
}

/// Clears `area` and draws a bordered box titled `title` on it.
fn frame(area: Rect, title: &str, buf: &mut Buffer) {
    Clear.render(area, buf);
    Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::Cyan))
        .render(area, buf);
}

fn inner(area: Rect) -> Rect {
    Block::bordered().inner(area)
}

/// A `width` x `height` rect in the middle of `area`, shrunk to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
