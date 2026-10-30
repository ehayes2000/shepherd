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

/// `text` in at most `width` columns, ending in `…` if it was cut. Nothing fits in none.
pub(crate) fn cut(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    match width.checked_sub(1) {
        Some(room) => head(text, room) + CUT,
        None => String::new(),
    }
}

/// `text` in at most `width` columns, starting with `…` if its start was cut. Nothing fits in none.
pub(crate) fn cut_left(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    match width.checked_sub(1) {
        Some(room) => CUT.to_owned() + &tail(text, room),
        None => String::new(),
    }
}

/// `text` followed by blanks to fill `width` columns.
pub(crate) fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(columns(text))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXTS: [&str; 8] = [
        "shepherd",
        "羊飼いの群れ",
        "👨\u{200d}👩\u{200d}👧👨\u{200d}👩\u{200d}👧",
        "cafe\u{301}e\u{301}",
        "🇯🇵🇺🇸🇯🇵",
        "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}",
        "a羊👨\u{200d}👩\u{200d}👧e\u{301}🇯🇵❤\u{fe0f}z",
        "",
    ];

    /// The ways `text` can be shortened whole graphemes at a time: its first `n`, and its last.
    fn prefixes_and_suffixes(text: &str) -> (Vec<String>, Vec<String>) {
        let all: Vec<String> = graphemes(text).into_iter().map(|(g, _)| g).collect();
        let prefixes = (0..=all.len()).map(|n| all[..n].concat()).collect();
        let suffixes = (0..=all.len()).map(|n| all[n..].concat()).collect();
        (prefixes, suffixes)
    }

    #[test]
    fn cutting_never_overflows_and_keeps_whole_graphemes() {
        for text in TEXTS {
            let (prefixes, suffixes) = prefixes_and_suffixes(text);
            for width in 0..=8 {
                let at = format!("{text:?} at {width}");
                let nothing = |shown: &str| width == 0 && shown.is_empty();

                let shown = cut(text, width);
                assert!(columns(&shown) <= width, "cut {at}: {shown:?}");
                let start = shown.strip_suffix(CUT);
                assert!(
                    shown == text
                        || nothing(&shown)
                        || start.is_some_and(|p| prefixes.iter().any(|q| q == p)),
                    "cut {at}: {shown:?}"
                );

                let shown = cut_left(text, width);
                assert!(columns(&shown) <= width, "cut_left {at}: {shown:?}");
                let end = shown.strip_prefix(CUT);
                assert!(
                    shown == text
                        || nothing(&shown)
                        || end.is_some_and(|s| suffixes.iter().any(|q| q == s)),
                    "cut_left {at}: {shown:?}"
                );

                let shown = tail(text, width);
                assert!(columns(&shown) <= width, "tail {at}: {shown:?}");
                assert!(suffixes.contains(&shown), "tail {at}: {shown:?}");

                assert_eq!(
                    columns(&pad(text, width)),
                    width.max(columns(text)),
                    "pad {at}"
                );
            }
        }
    }
}
