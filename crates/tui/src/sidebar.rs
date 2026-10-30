//! The session list on the left.
//!
//! Each session is two lines: its status glyph and name, then `machine · PR` (placeholders until M3 / M4). The
//! selected session has a highlighted background, and a session that wants the user has a yellow bar on its left
//! edge. The newest session is at the top, and the list scrolls to keep the selected one visible. The last line is a
//! footer: the prefix while it's pending, else the latest notice, else a reconnecting note, else a hint for help.
//!
//! Collapsed (narrow windows), only the attention bar and glyph are drawn, and the footer shows `^B` while the prefix
//! is pending.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use shepherd_core::client::{Client, ClientSession, Connection, Focus};
use shepherd_core::session::Activity;

use crate::layout::Layout;

/// Lines per session.
const ROW_HEIGHT: u16 = 2;
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

pub(crate) fn render(client: &Client, layout: &Layout, buf: &mut Buffer) {
    let area = layout.sidebar;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = Style::new().fg(Color::DarkGray);
    for y in area.top()..area.bottom() {
        buf[(area.right() - 1, y)].set_symbol("│").set_style(border);
    }
    // The area left of the border.
    let inner = Rect::new(area.x, area.y, area.width - 1, area.height);
    if layout.collapsed {
        render_collapsed(client, inner, buf);
    } else {
        render_full(client, inner, buf);
    }
}

fn render_full(client: &Client, area: Rect, buf: &mut Buffer) {
    buf.set_stringn(
        area.x + 1,
        area.y,
        "shepherd",
        usize::from(area.width.saturating_sub(1)),
        Style::new().add_modifier(Modifier::BOLD),
    );
    if area.height < 2 {
        return;
    }
    let list = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
    );
    rows(client, list, buf, |session, selected, line, buf| {
        let width = usize::from(line.width.saturating_sub(3));
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        let (symbol, color) = glyph(session);
        buf.set_string(line.x + 1, line.y, symbol, Style::new().fg(color));
        buf.set_stringn(line.x + 3, line.y, &session.name, width, style);
        let detail = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
        buf.set_stringn(line.x + 3, line.y + 1, PLACEHOLDER_DETAIL, width, detail);
    });
    footer(
        client,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        buf,
    );
}

fn render_collapsed(client: &Client, area: Rect, buf: &mut Buffer) {
    let list = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    rows(client, list, buf, |session, _, line, buf| {
        let (symbol, color) = glyph(session);
        buf.set_string(line.x + 1, line.y, symbol, Style::new().fg(color));
    });
    if client.focus() == Focus::Command && area.height > 0 {
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
    area: Rect,
    buf: &mut Buffer,
    mut draw: impl FnMut(&ClientSession, bool, Rect, &mut Buffer),
) {
    let visible = usize::from(area.height / ROW_HEIGHT);
    if visible == 0 || area.width < 2 {
        return;
    }
    let sessions = client.sessions();
    let selected = client
        .selected()
        .and_then(|id| sessions.iter().position(|s| &s.id == id));
    let offset = selected.map_or(0, |i| (i + 1).saturating_sub(visible));
    for (slot, (index, session)) in sessions
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let y = area.y + slot as u16 * ROW_HEIGHT;
        let line = Rect::new(area.x, y, area.width, ROW_HEIGHT);
        let is_selected = selected == Some(index);
        if is_selected {
            buf.set_style(line, Style::new().bg(SELECTED_BG));
        }
        if session.attention {
            for dy in 0..ROW_HEIGHT {
                buf.set_string(line.x, y + dy, ATTENTION_BAR, Style::new().fg(ATTENTION));
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
