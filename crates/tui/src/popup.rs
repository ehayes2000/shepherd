//! Popups drawn over everything: help, the rename input, the close confirmation, a session's context menu, and the
//! profile picker for a new session.
//!
//! Dialogs are centered, with their buttons right-aligned on their last line. The menu opens with its top-left corner
//! where it was asked for, moved in to fit the window. [`place`] says where a popup and its controls are, for both
//! drawing and [`crate::hit`].

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use shepherd_core::client::{Cell, Client, MenuItem, Popup};

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

/// The menu's items, with their labels as drawn.
const MENU: [(MenuItem, &str); 2] = [(MenuItem::Rename, " Rename"), (MenuItem::Close, " Close")];
/// The menu's width, its border included.
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
        Popup::Menu { at, .. } => menu(*at, window),
        Popup::PickProfile => pick_profile(client, window),
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
    Placed { area, controls }
}

/// The menu, its top-left corner at `at` or as near as fits in `window`. Each item is a whole line of it.
fn menu(at: Cell, window: Rect) -> Placed {
    let width = MENU_WIDTH.min(window.width);
    let height = (MENU.len() as u16 + 2).min(window.height);
    let x = at.col.min(window.right() - width).max(window.x);
    let y = at.row.min(window.bottom() - height).max(window.y);
    let area = Rect::new(x, y, width, height);
    let inside = inner(area);
    let controls = MENU
        .iter()
        .zip(inside.y..inside.bottom())
        .map(|((item, label), y)| Control {
            area: Rect::new(inside.x, y, inside.width, 1),
            target: Target::MenuItem(*item),
            label: (*label).to_owned(),
        })
        .collect();
    Placed { area, controls }
}

/// The profile picker: a prompt, then one row per profile (the default first and marked), each a whole line of the
/// dialog, then `[Cancel]`.
fn pick_profile(client: &Client, window: Rect) -> Placed {
    let profiles = client.profiles();
    let height = profiles.len() as u16 + 4;
    let mut placed = dialog(
        centered(window, DIALOG_WIDTH, height),
        &[("[Cancel]", Target::Dismiss)],
    );
    let inside = inner(placed.area);
    let name_width = profiles
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(0);
    let rows = (inside.y + 1)..inside.bottom().saturating_sub(1);
    for (profile, y) in profiles.iter().zip(rows) {
        let mark = if client.default_profile() == Some(profile.name.as_str()) {
            DEFAULT_MARK
        } else {
            ""
        };
        let label = format!(
            " {:<name_width$}  {:<11}  {mark}",
            profile.name,
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
    };
    let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    for control in &placed.controls {
        let style = match (popup, &control.target) {
            (Popup::Menu { .. }, _) | (_, Target::Profile(_)) => Style::new(),
            _ => button,
        };
        let at = control.area;
        buf.set_stringn(at.x, at.y, &control.label, usize::from(at.width), style);
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

/// The input and its hint, in `body`.
fn rename(name: &str, body: Rect, buf: &mut Buffer) -> Option<Position> {
    if body.width < 2 || body.height < 2 {
        return None;
    }
    // Keep the end of a long name in view, with a column for the cursor.
    let room = usize::from(body.width - 1);
    let chars: Vec<char> = name.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    let (end, _) = buf.set_stringn(body.x, body.y, &shown, room, Style::new());
    let hint = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    buf.set_stringn(
        body.x,
        body.y + 1,
        "enter to rename · esc to cancel",
        usize::from(body.width),
        hint,
    );
    Some(Position::new(end, body.y))
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
