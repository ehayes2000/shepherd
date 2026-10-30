//! The only code that hands text to ratatui. Every function here takes a [`DisplayText`], which only
//! [`display_safe`] makes, so text from agents, hooks, git and other clients can't reach the screen unsafe (F35).
//! `crates/tui/clippy.toml` forbids ratatui's own text sinks (`Buffer::set_string`, `Line::raw`, `Paragraph::new`,
//! ...) everywhere else.
#![allow(clippy::disallowed_methods)]

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use shepherd_core::display::{DisplayText, display_safe};

use crate::width::columns;

/// The most grapheme clusters of any one text the TUI shows: more than any row has columns.
const SHOWN_MAX: usize = 512;

/// Ends text cut short.
const CUT: &str = "…";

/// `text` made safe to show. Shepherd's own text goes through here too; it comes back as it is.
pub(crate) fn shown(text: &str) -> DisplayText {
    display_safe(text, SHOWN_MAX)
}

/// Draws `text` at `x`, `y` in at most `width` columns. Returns where it ended, as [`Buffer::set_stringn`] does.
pub(crate) fn set(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &DisplayText,
    width: usize,
    style: Style,
) -> (u16, u16) {
    buf.set_stringn(x, y, text.as_str(), width, style)
}

/// Draws `text` at `x`, `y` in at most `width` columns, ending in `…` when it doesn't fit.
pub(crate) fn set_cut(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &DisplayText,
    width: usize,
    style: Style,
) {
    if columns(text.as_str()) <= width {
        buf.set_stringn(x, y, text.as_str(), width, style);
        return;
    }
    let Some(room) = width.checked_sub(1) else {
        return;
    };
    let (end, _) = buf.set_stringn(x, y, text.as_str(), room, style);
    buf.set_stringn(end, y, CUT, 1, style);
}

pub(crate) fn span(text: DisplayText, style: Style) -> Span<'static> {
    Span::styled(text.into_string(), style)
}

pub(crate) fn line(text: DisplayText, style: Style) -> Line<'static> {
    Line::styled(text.into_string(), style)
}

pub(crate) fn paragraph(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
}

pub(crate) fn titled(block: Block<'static>, title: DisplayText) -> Block<'static> {
    block.title(title.into_string())
}
