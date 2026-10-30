//! The only code that hands text to ratatui. Every function here takes a [`DisplayText`], which only
//! [`display_safe`] makes, so text from agents, hooks, git and other clients can't reach the screen unsafe (F35).
//! `crates/tui/clippy.toml` forbids ratatui's own text sinks (`Buffer::set_string`, `Line::raw`, `Paragraph::new`,
//! `Widget::render`, ...) everywhere else, and `scripts/check-arch.sh` forbids `Line::from`, `Text::from` and
//! `Span::from`. Lines reach a paragraph only as a [`ShownLine`] made here, and widgets reach the buffer only through
//! [`render`], which takes just the widgets this module can fill.
#![allow(clippy::disallowed_methods)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
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

/// A line whose text all went through [`display_safe`]. Only [`line`] and [`spans`] make one.
#[derive(Clone, Debug)]
pub(crate) struct ShownLine(Line<'static>);

pub(crate) fn line(text: DisplayText, style: Style) -> ShownLine {
    ShownLine(Line::styled(text.into_string(), style))
}

/// One line of differently styled parts.
pub(crate) fn spans(parts: Vec<(DisplayText, Style)>) -> ShownLine {
    let spans: Vec<Span<'static>> = parts
        .into_iter()
        .map(|(text, style)| Span::styled(text.into_string(), style))
        .collect();
    ShownLine(Line::from(spans))
}

pub(crate) fn paragraph(lines: Vec<ShownLine>) -> Paragraph<'static> {
    Paragraph::new(lines.into_iter().map(|l| l.0).collect::<Vec<_>>())
}

pub(crate) fn titled(block: Block<'static>, title: DisplayText) -> Block<'static> {
    block.title(title.into_string())
}

/// Draws `widget` in `area`. Only widgets whose text this module put there can be drawn: a [`paragraph`], a block
/// ([`titled`] or untitled) and [`Clear`].
pub(crate) fn render(widget: impl Drawable, area: Rect, buf: &mut Buffer) {
    widget.render(area, buf);
}

/// The widgets [`render`] takes. Sealed: no other widget can be one.
pub(crate) trait Drawable: Widget + sealed::Sealed {}

impl Drawable for Paragraph<'static> {}
impl Drawable for Block<'static> {}
impl Drawable for Clear {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for ratatui::widgets::Paragraph<'static> {}
    impl Sealed for ratatui::widgets::Block<'static> {}
    impl Sealed for ratatui::widgets::Clear {}
}
