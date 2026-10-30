//! Session naming: the worktree (and branch) name a new session gets ([`worktree_name`]), and the display name its
//! first user prompt gives it ([`auto_name`]).
//!
//! The worktree name is a slug of the session's name, made unique against the branches the host has recorded.
//!
//! The slug is lowercase ASCII: letters and digits are kept (lowercased), `_` is kept inside the name, and every run
//! of anything else (spaces, punctuation, non-ASCII letters) becomes one `-`. Leading and trailing `-` and `_` are
//! dropped, and a name with nothing left is `session`. It is cut to fit [`MAX_LEN`] with room for a suffix. A taken
//! slug gets the first free `-2`, `-3`, ... Taken names compare without case, since a case-insensitive filesystem
//! can't hold worktree directories that differ only in case.
//!
//! Non-ASCII letters aren't transliterated (`café` → `caf`): that needs a transliteration table, which no allowed
//! crate provides.

use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use unicode_segmentation::UnicodeSegmentation;

use crate::ports::worktrees::WorktreeName;

/// The longest [`WorktreeName`].
pub const MAX_LEN: usize = 64;

/// What a slug with nothing left in it is called.
const FALLBACK: &str = "session";

/// The worktree name for a session called `session`, unique against `taken` (compared without case).
pub fn worktree_name(session: &str, taken: &BTreeSet<String>) -> WorktreeName {
    let taken: BTreeSet<String> = taken.iter().map(|t| t.to_ascii_lowercase()).collect();
    let slug = slug(session);
    let candidate = (1..)
        .map(|n| suffixed(&slug, n))
        .find(|c| !taken.contains(c))
        .unwrap_or_else(|| unreachable!("a finite set can't hold every suffix"));
    WorktreeName::new(candidate)
        .unwrap_or_else(|e| unreachable!("a slug is always a valid worktree name: {e}"))
}

/// `session` as lowercase ASCII, see the module docs. Never empty, at most [`MAX_LEN`] long.
fn slug(session: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in session.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    let out = trim(&out);
    if out.is_empty() {
        FALLBACK.to_owned()
    } else {
        cut(out, MAX_LEN).to_owned()
    }
}

/// `slug` with suffix `-n` (none for 1), cut so the whole fits in [`MAX_LEN`].
fn suffixed(slug: &str, n: u32) -> String {
    if n == 1 {
        return slug.to_owned();
    }
    let suffix = format!("-{n}");
    let base = cut(slug, MAX_LEN - suffix.len());
    format!("{base}{suffix}")
}

/// The first `len` bytes of ASCII `s`, without a trailing separator.
fn cut(s: &str, len: usize) -> &str {
    trim(&s[..s.len().min(len)])
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c| c == '-' || c == '_')
}

/// The most words an [`auto_name`] has.
pub const AUTO_MAX_WORDS: usize = 4;

/// The most characters an [`auto_name`] has, counted as user-perceived characters (grapheme clusters).
pub const AUTO_MAX_CHARS: usize = 30;

/// The most bytes an [`auto_name`] has.
pub const AUTO_MAX_BYTES: usize = 120;

/// The most combining marks an [`auto_name`] keeps after each base character.
const MAX_MARKS: usize = 2;

/// The punctuation UAX #29 keeps inside a word that an [`auto_name`] keeps too, never at a word's ends.
const IN_WORD: [char; 4] = ['.', ':', ',', '_'];

/// Words a prompt is full of that say nothing about its task, lowercase and without apostrophes.
const STOPWORDS: &[&str] = &[
    "a", "about", "all", "also", "am", "an", "and", "any", "are", "arent", "as", "at", "be",
    "been", "being", "but", "by", "can", "cant", "could", "couldnt", "did", "didnt", "do", "does",
    "doesnt", "doing", "done", "dont", "for", "from", "had", "has", "have", "he", "hello", "help",
    "her", "here", "hey", "hi", "his", "how", "i", "id", "if", "ill", "im", "in", "into", "is",
    "isnt", "it", "its", "ive", "just", "let", "lets", "like", "may", "me", "might", "mine",
    "must", "my", "need", "needs", "not", "now", "of", "ok", "okay", "on", "onto", "or", "our",
    "please", "pls", "plz", "really", "she", "should", "shouldnt", "so", "some", "thank", "thanks",
    "that", "thats", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "to", "us", "very", "want", "wanna", "was", "we", "were", "what", "whats", "when", "where",
    "which", "who", "why", "will", "with", "wont", "would", "wouldnt", "you", "youd", "youll",
    "your", "youre", "yours",
];

/// The display name shepherd gives a session after its first user prompt, or `None` when the prompt has no
/// meaningful words. Only the display name: the branch keeps the name it was made with.
///
/// The prompt is NFC-normalized. Fenced code blocks are dropped: a line whose first non-blank characters are
/// ```` ``` ```` or `~~~`, at any indentation, opens or closes one. So is every whitespace-separated token with a `/` or
/// `\` in it (paths, URLs). Apostrophes are removed, so a contraction is one word. Words are the Unicode word segments
/// (UAX #29), with segments that touch joined (UAX #29 splits unspaced ideographs one by one), lowercased. Inside a
/// word only letters and digits are kept, each with at most [`MAX_MARKS`] combining marks after it, plus the in-word
/// punctuation `.`, `:`, `,` and `_`, never at a word's ends. Everything else is dropped: controls, invisible format
/// characters (bidi controls, joiners, soft hyphens, BOMs, tags), symbols. The printable remains of a terminal escape
/// sequence (`31m` of `ESC [31m`) can be words. [`STOPWORDS`] are dropped.
///
/// The name is the first words joined with `-`, up to [`AUTO_MAX_WORDS`] of them, [`AUTO_MAX_CHARS`] grapheme
/// clusters and [`AUTO_MAX_BYTES`] bytes: it stops at the first word that doesn't fit, even if a later one would. A
/// first word too long on its own is cut, never inside a grapheme cluster, and loses any in-word punctuation the cut
/// left at its end.
pub fn auto_name(prompt: &str) -> Option<String> {
    let prompt: String = prompt.nfc().collect();
    let mut name = String::new();
    let mut len = 0;
    for (n, word) in prompt_words(&prompt).take(AUTO_MAX_WORDS).enumerate() {
        let word_len = word.graphemes(true).count();
        if n == 0 {
            for grapheme in word.graphemes(true) {
                if len == AUTO_MAX_CHARS || name.len() + grapheme.len() > AUTO_MAX_BYTES {
                    break;
                }
                name.push_str(grapheme);
                len += 1;
            }
            let kept = name.trim_end_matches(IN_WORD).len();
            name.truncate(kept);
        } else if len + 1 + word_len <= AUTO_MAX_CHARS
            && name.len() + 1 + word.len() <= AUTO_MAX_BYTES
        {
            name.push('-');
            name.push_str(&word);
            len += 1 + word_len;
        } else {
            break;
        }
    }
    (!name.is_empty()).then_some(name)
}

/// The meaningful words of `prompt`, in order, see [`auto_name`].
fn prompt_words(prompt: &str) -> impl Iterator<Item = String> + '_ {
    let mut fenced = false;
    prompt
        .lines()
        .filter(move |line| {
            let line = line.trim_start();
            if line.starts_with("```") || line.starts_with("~~~") {
                fenced = !fenced;
                return false;
            }
            !fenced
        })
        .flat_map(str::split_whitespace)
        .filter(|token| !token.contains(['/', '\\']))
        .flat_map(|token| token_words(&token.replace(['\'', '\u{2019}'], "")))
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
}

/// The words of one token, lowercased and cleaned ([`displayable`]): its word segments, with segments that touch
/// (nothing between them, as UAX #29 leaves each ideograph and kana run of unspaced text) joined into one word.
fn token_words(token: &str) -> Vec<String> {
    let mut words: Vec<(usize, String)> = Vec::new();
    for (start, segment) in token.unicode_word_indices() {
        match words.last_mut() {
            Some((end, word)) if *end == start => {
                word.push_str(segment);
                *end += segment.len();
            }
            _ => words.push((start + segment.len(), segment.to_owned())),
        }
    }
    words
        .into_iter()
        .map(|(_, w)| displayable(&w.to_lowercase()))
        .filter(|w| !w.is_empty())
        .collect()
}

/// `word` with only letters and digits (each with at most [`MAX_MARKS`] combining marks) and [`IN_WORD`] punctuation
/// inside, see [`auto_name`]. Empty if it has no letter or digit.
fn displayable(word: &str) -> String {
    let mut out = String::new();
    // Combining marks still allowed after the last kept character: none before the first base.
    let mut marks = 0;
    for c in word.chars() {
        // Marks first: some (Devanagari vowel signs, Greek ypogegrammeni) are alphabetic too.
        if is_combining_mark(c) {
            if marks > 0 {
                out.push(c);
                marks -= 1;
            }
        } else if c.is_alphanumeric() {
            out.push(c);
            marks = MAX_MARKS;
        } else if IN_WORD.contains(&c) {
            out.push(c);
            marks = 0;
        }
    }
    out.trim_matches(IN_WORD).to_owned()
}
