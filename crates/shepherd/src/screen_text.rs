//! A terminal's text as a user reads it: each row display-safe, without trailing blanks, a wide character once (the
//! cell it covers skipped). The host answers `_shepherd/session/screen` with [`screen_text`].

use shepherd_core::display::display_safe;
use shepherd_core::wire::SessionScreenOk;
use shepherd_term::{Cell, Screen, Width};

/// The text of one row, from each cell's symbol and how many columns it takes, left to right: each symbol
/// display-safe (one that is left empty shows as a blank), the cells a wide character covers skipped, and trailing
/// blanks trimmed.
pub fn row_text<S: AsRef<str>>(cells: impl IntoIterator<Item = (S, u16)>) -> String {
    let mut line = String::new();
    let mut skip: u16 = 0;
    for (symbol, width) in cells {
        if skip == 0 {
            let symbol = display_safe(symbol.as_ref(), 1).into_string();
            line.push_str(if symbol.is_empty() { " " } else { &symbol });
        }
        skip = skip.max(width).saturating_sub(1);
    }
    line.trim_end().to_owned()
}

/// What `screen` shows, as text: up to `scrollback` lines of its scrollback (oldest first), then each of its rows.
/// The newest lines are kept first: once the text (its lines joined with line breaks) would pass `max_bytes`, the
/// older lines are left out and the answer says it was truncated. Only the rows kept, and the one
/// that didn't fit, are read.
pub fn screen_text(screen: &Screen, scrollback: u32, max_bytes: usize) -> SessionScreenOk {
    let (cols, rows) = screen.size();
    let kept = screen
        .scrollback()
        .min(usize::try_from(scrollback).unwrap_or(usize::MAX));
    let newest_first = (0..rows)
        .rev()
        .map(|row| (false, cells_text(screen.row(row))))
        .chain((1..=kept).map(|back| (true, cells_text(screen.scrollback_row(back)))));
    let mut lines = Vec::new();
    let mut from_scrollback = 0u32;
    let mut bytes = 0usize;
    let mut truncated = false;
    for (is_scrollback, line) in newest_first {
        let line_break = usize::from(!lines.is_empty());
        bytes = bytes.saturating_add(line.len() + line_break);
        if bytes > max_bytes {
            truncated = true;
            break;
        }
        from_scrollback += u32::from(is_scrollback);
        lines.push(line);
    }
    lines.reverse();
    SessionScreenOk {
        cols,
        rows,
        scrollback: from_scrollback,
        lines,
        truncated,
    }
}

fn cells_text<'a>(cells: impl Iterator<Item = Cell<'a>>) -> String {
    row_text(cells.map(|cell| {
        let base = if cell.ch.is_control() { ' ' } else { cell.ch };
        let symbol: String = std::iter::once(base)
            .chain(cell.zerowidth.iter().copied())
            .collect();
        let width = match cell.width {
            Width::Wide => 2,
            Width::Single | Width::Spacer => 1,
        };
        (symbol, width)
    }))
}
