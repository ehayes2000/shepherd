//! Text from outside shepherd's own code, made safe to show: see [`display_safe`].
//!
//! Session names (auto or renamed by any client), failure reasons, hook warnings, git errors, notices and paths all
//! come from somewhere shepherd doesn't control. Before any of it is drawn or put in a desktop notification it goes
//! through [`display_safe`], and the TUI only draws a [`DisplayText`].

use std::fmt;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

/// The most combining marks kept after each base character.
const MAX_MARKS: usize = 2;

/// How many chars of its input [`display_safe`] reads, at most, for each grapheme cluster it may keep. Past that the
/// rest is never looked at, so the work is bounded by the cap, not the input.
pub const CHARS_READ_PER_GRAPHEME: usize = 32;

/// Text that went through [`display_safe`]: no control or format characters, NFC, a bounded number of combining marks
/// per base, and a bounded number of grapheme clusters. Only [`display_safe`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayText(String);

impl DisplayText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DisplayText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `text` made safe to show, in at most `max` grapheme clusters.
///
/// Every character of category Cc (controls: escape, BEL, newline, tab, C1) or Cf (format: bidi overrides, embeddings,
/// isolates and marks, zero-width joiners and space, BOM, soft hyphen, tags) is dropped, so text can't reorder what
/// is shown around it, hide what it says, or end an escape sequence it's put in. What's left is NFC-normalized and
/// keeps at most two combining marks after each base character (none before the first), then is cut to its first
/// `max` grapheme clusters. Only the first `max` × [`CHARS_READ_PER_GRAPHEME`] chars of `text` are read.
///
/// Text that is already safe comes back as it is (for the same `max`), so safe text that was cut or joined with more
/// safe text can be made a [`DisplayText`] again.
pub fn display_safe(text: &str, max: usize) -> DisplayText {
    display_safe_iter(text.chars(), max)
}

/// [`display_safe`] for text that arrives as chars.
pub fn display_safe_iter(chars: impl IntoIterator<Item = char>, max: usize) -> DisplayText {
    let visible = chars
        .into_iter()
        .take(max.saturating_mul(CHARS_READ_PER_GRAPHEME))
        .filter(|&c| !invisible(c));
    let mut out = String::new();
    // Combining marks still allowed after the last base: none before the first.
    let mut marks = 0;
    for c in visible.nfc() {
        if is_combining_mark(c) {
            if marks > 0 {
                out.push(c);
                marks -= 1;
            }
        } else {
            out.push(c);
            marks = MAX_MARKS;
        }
    }
    let end = out
        .grapheme_indices(true)
        .nth(max)
        .map_or(out.len(), |(at, _)| at);
    out.truncate(end);
    DisplayText(out)
}

/// Whether `c` is a control (Cc) or format (Cf) character.
fn invisible(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::Control | GeneralCategory::Format
    )
}
