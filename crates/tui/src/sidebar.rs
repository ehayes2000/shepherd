//! The session list on the left.
//!
//! The first two lines are the header: the workspace (`⌂ <name>`, or `⌂ choose a workspace`), which opens the
//! switcher, then a `+ New session` button (`⌂` and `+` when collapsed). Each session is two lines: its status glyph,
//! name and a `×` close button at the right edge, then `machine · PR` (placeholders until M3 / M4). A session from
//! another workspace is dimmed, and its second line says which workspace instead. The selected session has a
//! highlighted background, and a session that wants the user has a yellow bar on its left edge.
//! The newest session is at the top. The list starts at the client's [`Client::sidebar_scroll`], which keeps the
//! selected session visible when the selection changes and follows the wheel otherwise. The last line is a footer:
//! the prefix and its bindings while it's pending; otherwise the latest notice, else a reconnecting note, else a hint
//! for help, then the `Detach` and `?` buttons on the right.
//!
//! Collapsed (narrow windows), only the `+`, the attention bar and glyph are drawn, with the `⏏` (detach) and `?`
//! buttons on the last two lines; the footer shows `^B` instead while the prefix is pending. Rows have no `×` there:
//! a right click opens the row's menu, which has Close.
//!
//! [`crate::hit`] reads the same geometry ([`workspace_button`], [`new_session_button`], [`visible_rows`],
//! [`close_button`], [`footer_buttons`]) that drawing uses.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use shepherd_core::client::{Client, ClientSession, Connection, Focus};
use shepherd_core::session::Activity;

use crate::layout::{Layout, Target};

/// Lines per session.
const ROW_HEIGHT: u16 = 2;
/// Lines above the list: the workspace and `+ New session`.
const HEADER_HEIGHT: u16 = 2;
const SELECTED_BG: Color = Color::DarkGray;
const ATTENTION: Color = Color::Yellow;
const ATTENTION_BAR: &str = "▌";
/// What line 2 says until sessions know their machine (M4) and PR (M3).
const PLACEHOLDER_DETAIL: &str = "local · —";

/// A session's status glyph and its colour. See the table in the crate docs.
pub fn glyph(session: &ClientSession) -> (&'static str, Color) {
    if session.exited {
        return ("✕", Color::Red);
    }
    match session.activity {
        Activity::Starting => ("◌", Color::DarkGray),
        Activity::Working { .. } => ("●", Color::Green),
        Activity::NeedsPermission { .. } | Activity::NeedsInput => ("▲", Color::Yellow),
        Activity::Idle => ("○", Color::Reset),
        Activity::Exited { .. } => ("✕", Color::Red),
        Activity::Unknown => ("?", Color::DarkGray),
    }
}

/// The button's label, and the collapsed sidebar's.
const NEW_SESSION: &str = "+ New session";
const NEW_SESSION_COLLAPSED: &str = "+";

/// The sidebar left of its border.
fn inner(layout: &Layout) -> Rect {
    let area = layout.sidebar;
    Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height)
}

/// The workspace's label: its directory's name, or an offer to choose one.
const WORKSPACE_MARK: &str = "⌂";
const NO_WORKSPACE: &str = "⌂ choose a workspace";

fn workspace_label(client: &Client, layout: &Layout) -> String {
    if layout.collapsed {
        return WORKSPACE_MARK.to_owned();
    }
    match client.workspace() {
        Some(path) => format!("{WORKSPACE_MARK} {}", workspace_name(path)),
        None => NO_WORKSPACE.to_owned(),
    }
}

/// What a workspace is called in the sidebar: its directory's name.
fn workspace_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// A label on header line `line`, a column in from the left, cut to fit.
fn header_button(layout: &Layout, line: u16, label: &str) -> Rect {
    let inner = inner(layout);
    if inner.width < 2 || inner.height <= line {
        return Rect::default();
    }
    let width = (label.chars().count() as u16).min(inner.width - 1);
    Rect::new(inner.x + 1, inner.y + line, width, 1)
}

/// Where the workspace button is: its label, on the first line.
pub(crate) fn workspace_button(client: &Client, layout: &Layout) -> Rect {
    header_button(layout, 0, &workspace_label(client, layout))
}

/// Where the `+ New session` button is: its label, on the second line.
pub(crate) fn new_session_button(layout: &Layout) -> Rect {
    let label = if layout.collapsed {
        NEW_SESSION_COLLAPSED
    } else {
        NEW_SESSION
    };
    header_button(layout, 1, label)
}

/// A row's close button, drawn as `×`.
const CLOSE: &str = "×";
const DETACH: &str = "Detach";
const HELP: &str = "?";
/// The collapsed sidebar's `Detach` button.
const DETACH_COLLAPSED: &str = "⏏";

/// Lines below the list: the footer, and in the collapsed sidebar the `⏏` line above it.
fn footer_height(layout: &Layout) -> u16 {
    if layout.collapsed { 2 } else { 1 }
}

/// Where the close button of the session drawn on `row` is: the last cell of its first line. The collapsed sidebar
/// has none.
pub(crate) fn close_button(layout: &Layout, row: Rect) -> Rect {
    if layout.collapsed || row.width < 6 || row.height == 0 {
        return Rect::default();
    }
    Rect::new(row.right() - 1, row.y, 1, 1)
}

/// The footer's buttons, left to right: `Detach`, then `?` in the last column. Collapsed, they are glyphs in the
/// glyph column: `⏏` just above `?` on the last line. Not shown while the prefix is pending, when the footer shows
/// it instead.
pub(crate) fn footer_buttons(
    client: &Client,
    layout: &Layout,
) -> Vec<(Rect, Target, &'static str)> {
    let inner = inner(layout);
    if client.focus() == Focus::Command {
        return Vec::new();
    }
    if layout.collapsed {
        if inner.height < HEADER_HEIGHT + 2 || inner.width < 2 {
            return Vec::new();
        }
        let help = Rect::new(inner.x + 1, inner.bottom() - 1, 1, 1);
        let detach = Rect {
            y: help.y - 1,
            ..help
        };
        return vec![
            (detach, Target::Detach, DETACH_COLLAPSED),
            (help, Target::Help, HELP),
        ];
    }
    let width = DETACH.chars().count() as u16;
    if inner.height < HEADER_HEIGHT + 1 || inner.width < width + 6 {
        return Vec::new();
    }
    let y = inner.bottom() - 1;
    let help = Rect::new(inner.right() - 1, y, 1, 1);
    let detach = Rect::new(help.x - 1 - width, y, width, 1);
    vec![(detach, Target::Detach, DETACH), (help, Target::Help, HELP)]
}

/// The session rows, between the header and the footer.
fn list(layout: &Layout) -> Rect {
    let inner = inner(layout);
    Rect::new(
        inner.x,
        inner.y.saturating_add(HEADER_HEIGHT),
        inner.width,
        inner
            .height
            .saturating_sub(HEADER_HEIGHT + footer_height(layout)),
    )
}

/// How many sessions the sidebar of `layout` shows at once.
pub(crate) fn capacity(layout: &Layout) -> usize {
    let list = list(layout);
    if list.width < 2 {
        return 0;
    }
    usize::from(list.height / ROW_HEIGHT)
}

/// Each session shown, as its index in the client's list and its two lines, top to bottom, from the client's scroll.
pub(crate) fn visible_rows(
    client: &Client,
    layout: &Layout,
) -> impl Iterator<Item = (usize, Rect)> {
    let list = list(layout);
    let visible = capacity(layout);
    let len = client.sessions().len();
    let first = client.sidebar_scroll().min(len.saturating_sub(visible));
    (first..len.min(first + visible))
        .zip(0u16..)
        .map(move |(index, slot)| {
            let y = list.y + slot * ROW_HEIGHT;
            (index, Rect::new(list.x, y, list.width, ROW_HEIGHT))
        })
}

pub(crate) fn render(client: &Client, layout: &Layout, buf: &mut Buffer) {
    let area = layout.sidebar;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = Style::new().fg(Color::DarkGray);
    for y in area.top()..area.bottom() {
        buf[(area.right() - 1, y)].set_symbol("│").set_style(border);
    }
    let workspace = workspace_button(client, layout);
    let style = match client.workspace() {
        Some(_) => Style::new().add_modifier(Modifier::BOLD),
        None => Style::new().fg(Color::Gray),
    };
    let label = workspace_label(client, layout);
    buf.set_stringn(
        workspace.x,
        workspace.y,
        label,
        usize::from(workspace.width),
        style,
    );
    let button = new_session_button(layout);
    let label = if layout.collapsed {
        NEW_SESSION_COLLAPSED
    } else {
        NEW_SESSION
    };
    let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    buf.set_stringn(button.x, button.y, label, usize::from(button.width), style);
    if layout.collapsed {
        render_collapsed(client, layout, buf);
    } else {
        render_full(client, layout, buf);
    }
}

fn render_full(client: &Client, layout: &Layout, buf: &mut Buffer) {
    rows(client, layout, buf, |session, selected, line, buf| {
        let width = usize::from(line.width.saturating_sub(3));
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        if session.elsewhere {
            style = style.fg(Color::Gray).add_modifier(Modifier::DIM);
        }
        let (symbol, color) = glyph(session);
        buf.set_string(line.x + 1, line.y, symbol, Style::new().fg(color));
        // The name stops a column short of the close button.
        let name_width = usize::from(line.width.saturating_sub(5));
        buf.set_stringn(line.x + 3, line.y, &session.name, name_width, style);
        let close = close_button(layout, line);
        buf.set_stringn(
            close.x,
            close.y,
            CLOSE,
            usize::from(close.width),
            Style::new().fg(Color::Gray),
        );
        let detail = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
        let text = match &session.workspace {
            Some(workspace) if session.elsewhere => format!("in {}", workspace_name(workspace)),
            _ => PLACEHOLDER_DETAIL.to_owned(),
        };
        buf.set_stringn(line.x + 3, line.y + 1, text, width, detail);
    });
    let area = inner(layout);
    if area.height > HEADER_HEIGHT {
        let buttons = footer_buttons(client, layout);
        let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        for (at, _, label) in &buttons {
            buf.set_stringn(at.x, at.y, label, usize::from(at.width), button);
        }
        // The status keeps a blank column before the buttons.
        let line = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let status = match buttons.first() {
            Some((detach, ..)) => Rect {
                width: detach.x.saturating_sub(line.x + 1),
                ..line
            },
            None => line,
        };
        footer(client, status, buf);
    }
}

fn render_collapsed(client: &Client, layout: &Layout, buf: &mut Buffer) {
    rows(client, layout, buf, |session, _, line, buf| {
        let (symbol, color) = glyph(session);
        buf.set_string(line.x + 1, line.y, symbol, Style::new().fg(color));
    });
    let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    for (at, _, label) in footer_buttons(client, layout) {
        buf.set_stringn(at.x, at.y, label, usize::from(at.width), button);
    }
    let area = inner(layout);
    if client.focus() == Focus::Command && area.height > HEADER_HEIGHT {
        let style = Style::new().add_modifier(Modifier::REVERSED);
        buf.set_stringn(
            area.x,
            area.bottom() - 1,
            "^B",
            usize::from(area.width),
            style,
        );
    }
}

/// Draws each visible session's two lines with `draw`, after the selection highlight and attention bar.
fn rows(
    client: &Client,
    layout: &Layout,
    buf: &mut Buffer,
    mut draw: impl FnMut(&ClientSession, bool, Rect, &mut Buffer),
) {
    let sessions = client.sessions();
    for (index, line) in visible_rows(client, layout) {
        let session = &sessions[index];
        let is_selected = client.selected() == Some(&session.id);
        if is_selected {
            buf.set_style(line, Style::new().bg(SELECTED_BG));
        }
        if session.attention {
            for dy in 0..ROW_HEIGHT {
                buf.set_string(
                    line.x,
                    line.y + dy,
                    ATTENTION_BAR,
                    Style::new().fg(ATTENTION),
                );
            }
        }
        draw(session, is_selected, line, buf);
    }
}

fn footer(client: &Client, area: Rect, buf: &mut Buffer) {
    let width = usize::from(area.width.saturating_sub(1));
    let (x, y) = (area.x + 1, area.y);
    if client.focus() == Focus::Command {
        let (end, _) = buf.set_stringn(
            x,
            y,
            "ctrl-b",
            width,
            Style::new().add_modifier(Modifier::REVERSED),
        );
        let rest = width.saturating_sub(usize::from(end - x));
        buf.set_stringn(end, y, " n j k r x ? q", rest, Style::new().fg(Color::Gray));
    } else if let Some(notice) = client.notice() {
        buf.set_stringn(x, y, notice, width, Style::new().fg(ATTENTION));
    } else if *client.connection() != Connection::Ready && !client.sessions().is_empty() {
        buf.set_stringn(x, y, "reconnecting…", width, Style::new().fg(ATTENTION));
    } else {
        let hint = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
        buf.set_stringn(x, y, "ctrl-b ? help", width, hint);
    }
}
