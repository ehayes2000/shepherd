//! The worktree (and branch) name a new session gets: a slug of its name, made unique against the branches the host
//! has recorded.
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
