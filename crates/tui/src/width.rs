//! Text measured in terminal columns, not characters: a CJK character or an emoji takes two. Text is only ever cut
//! between graphemes, so an emoji of several characters is kept whole or left out.

use ratatui::style::Style;
use ratatui::text::Span;

/// Ends or starts text that was cut short.
const CUT: &str = "…";

/// How many columns `text` takes.
pub(crate) fn columns(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text`'s graphemes, each with the columns it takes.
fn graphemes(text: &str) -> Vec<(String, usize)> {
    Span::raw(text)
        .styled_graphemes(Style::new())
        .map(|g| (g.symbol.to_owned(), columns(g.symbol)))
        .collect()
}

/// As much of the start of `text` as fits in `width` columns.
fn head(text: &str, width: usize) -> String {
    let mut used = 0;
    graphemes(text)
        .into_iter()
        .take_while(|(_, w)| {
            used += w;
            used <= width
        })
        .map(|(g, _)| g)
        .collect()
}

/// As much of the end of `text` as fits in `width` columns.
pub(crate) fn tail(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut kept: Vec<String> = graphemes(text)
        .into_iter()
        .rev()
        .take_while(|(_, w)| {
            used += w;
            used <= width
        })
        .map(|(g, _)| g)
        .collect();
    kept.reverse();
    kept.concat()
}

/// `text` in at most `width` columns, ending in `…` if it was cut.
pub(crate) fn cut(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    head(text, width.saturating_sub(1)) + CUT
}

/// `text` in at most `width` columns, starting with `…` if its start was cut.
pub(crate) fn cut_left(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    CUT.to_owned() + &tail(text, width.saturating_sub(1))
}

/// `text` followed by blanks to fill `width` columns.
pub(crate) fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(columns(text))))
}
