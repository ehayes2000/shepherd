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

/// The most chars [`display_safe`] keeps in one grapheme cluster. Extenders that aren't combining marks (emoji
/// modifiers, halfwidth voiced marks, Hangul jamo, Indic conjuncts) could otherwise grow one cluster without bound.
/// At most [`CHARS_READ_PER_GRAPHEME`], so a second pass reads all of the first one's output.
pub const CHARS_KEPT_PER_GRAPHEME: usize = 16;

const _: () = assert!(CHARS_KEPT_PER_GRAPHEME <= CHARS_READ_PER_GRAPHEME);

/// Text that went through [`display_safe`]: no control, format, line or paragraph separator characters, NFC, a bounded
/// number of combining marks per base and of chars per grapheme cluster, and a bounded number of grapheme clusters.
/// Only [`display_safe`] makes one.
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
/// isolates and marks, zero-width joiners and space, BOM, soft hyphen, tags), Zl or Zp (U+2028, U+2029) is dropped,
/// so text can't reorder what is shown around it, hide what it says, break a line, or end an escape sequence it's put
/// in. What's left is NFC-normalized and keeps at most two combining marks after each base character (none before the
/// first) and at most [`CHARS_KEPT_PER_GRAPHEME`] chars in each grapheme cluster, then is cut to its first `max`
/// grapheme clusters. Only the first `max` × [`CHARS_READ_PER_GRAPHEME`] chars of `text` are read.
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
    let mut out = Clusters::default();
    // Combining marks still allowed after the last base: none before the first.
    let mut marks = 0;
    for c in visible.nfc() {
        if is_combining_mark(c) {
            if marks > 0 && out.push(c) {
                marks -= 1;
            }
        } else if out.push(c) {
            marks = MAX_MARKS;
        }
    }
    let mut out = out.text;
    let end = out
        .grapheme_indices(true)
        .nth(max)
        .map_or(out.len(), |(at, _)| at);
    out.truncate(end);
    DisplayText(out)
}

/// Text whose grapheme clusters each hold at most [`CHARS_KEPT_PER_GRAPHEME`] chars.
#[derive(Default)]
struct Clusters {
    text: String,
    /// Where the last cluster starts.
    last: usize,
}

impl Clusters {
    /// Appends `c` unless that makes the last cluster too long, or fills it with a char that joins whatever follows it
    /// (a Prepend), which would swallow the rest of the text. Returns whether it did.
    ///
    /// Appending only ever changes the last cluster (UAX #29 never looks ahead), and whether `c` joins it can be told
    /// from the last cluster alone (every rule that looks back stays inside one cluster, and regional indicators pair
    /// from a cluster's start), so each check segments only the last cluster, not the whole text.
    fn push(&mut self, c: char) -> bool {
        let at = self.text.len();
        self.text.push(c);
        let mut clusters = self.text[self.last..].graphemes(true);
        let first = clusters.next().unwrap_or_default();
        if clusters.next().is_some() {
            self.last = at;
            return true;
        }
        let mut chars = first.chars();
        let fits = match chars.nth(CHARS_KEPT_PER_GRAPHEME - 1) {
            None => true,
            Some(_) if chars.next().is_some() => false,
            Some(_) => !joins_anything(first),
        };
        if !fits {
            self.text.pop();
        }
        fits
    }
}

/// Whether any char appended to `cluster` would join it: a cluster ending in a Prepend.
fn joins_anything(cluster: &str) -> bool {
    let probe = format!("{cluster}a");
    probe.graphemes(true).nth(1).is_none()
}

/// Whether `c` is a control (Cc), format (Cf), line separator (Zl) or paragraph separator (Zp) character.
fn invisible(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    )
}
