//! The finder's matching and ranking: does a query match a name, and how well. Pure functions over text.
//!
//! Text is compared grapheme cluster by grapheme cluster (UAX #29), after NFC normalization and `to_lowercase`, so a
//! query never matches half of a character a user sees, composed and decomposed spellings match, and case is ignored
//! beyond ASCII. A match is, best first ([`Rank`]): a prefix of the text, a prefix of one of its words, or a
//! subsequence of it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use super::ClientSession;
use crate::ids::{MachineId, SessionId};

/// How well a query matches a text, best first: `Prefix < WordBoundary < Subsequence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// The text starts with the query. An empty query matches every text this way.
    Prefix,
    /// One of the text's words, after the first, starts with the query.
    WordBoundary,
    /// The query's grapheme clusters appear in the text in order, not necessarily together.
    Subsequence,
}

/// How well `query` matches `candidate`, or `None` if it doesn't.
///
/// A word starts where UAX #29 starts a segment, or after a grapheme cluster that isn't alphanumeric (so `_`, which
/// UAX #29 keeps inside words, separates them too).
pub fn rank(query: &str, candidate: &str) -> Option<Rank> {
    let query: String = query.nfc().collect();
    let query: Vec<String> = clusters_of(&query).map(|(_, g)| g).collect();
    if query.is_empty() {
        return Some(Rank::Prefix);
    }
    let text: String = candidate.nfc().collect();
    let clusters: Vec<(usize, String)> = clusters_of(&text).collect();
    let starts_at = |from: usize| {
        clusters.len() >= from + query.len()
            && clusters[from..from + query.len()]
                .iter()
                .zip(&query)
                .all(|((_, g), q)| g == q)
    };
    if starts_at(0) {
        return Some(Rank::Prefix);
    }
    let segment_starts: Vec<usize> = text.split_word_bound_indices().map(|(at, _)| at).collect();
    let word_start = |i: usize| {
        let (at, g) = &clusters[i];
        alphanumeric(g) && (segment_starts.contains(at) || !alphanumeric(&clusters[i - 1].1))
    };
    if (1..clusters.len()).any(|i| word_start(i) && starts_at(i)) {
        return Some(Rank::WordBoundary);
    }
    let mut rest = clusters.iter().map(|(_, g)| g);
    query
        .iter()
        .all(|q| rest.any(|g| g == q))
        .then_some(Rank::Subsequence)
}

/// The grapheme clusters of `text`, already NFC, lowercased, with where each starts.
fn clusters_of(text: &str) -> impl Iterator<Item = (usize, String)> + '_ {
    text.grapheme_indices(true)
        .map(|(at, g)| (at, g.to_lowercase()))
}

/// Whether a grapheme cluster starts with a letter or digit.
fn alphanumeric(cluster: &str) -> bool {
    cluster.chars().next().is_some_and(char::is_alphanumeric)
}

/// A managed worktree of the current workspace that no open session runs in, as the host listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub path: PathBuf,
    /// `None` when its HEAD is detached.
    pub branch: Option<String>,
}

/// The orphan worktrees, as far as the open finder knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Orphans {
    /// The host has been asked.
    Loading,
    /// The host listed these, in its order.
    Listed(Vec<Orphan>),
    /// They couldn't be listed, for this reason, for the user.
    Failed(String),
}

/// One result of the finder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderItem {
    /// A listed session: choosing it selects it.
    Session(SessionId),
    /// An orphan worktree: choosing it offers to start a session in it.
    Worktree(Orphan),
}

/// What the finder shows for `query`: the sessions (newest first) and then the orphan worktrees that match it, best
/// match first and in that order within one [`Rank`]. A session matches by its name, branch, workspace (its directory's
/// name) or `machine`, a worktree by its branch or directory name, each as well as its best-matching field. The query
/// is trimmed; an empty one matches everything.
pub(super) fn results(
    query: &str,
    sessions: &[ClientSession],
    machine: Option<&MachineId>,
    orphans: Option<&Orphans>,
) -> Vec<FinderItem> {
    let query = query.trim();
    let best =
        |fields: &[Option<&str>]| fields.iter().flatten().filter_map(|f| rank(query, f)).min();
    let mut ranked: Vec<(Rank, FinderItem)> = Vec::new();
    for s in sessions {
        let workspace = s.workspace.as_deref().and_then(dir_name);
        let fields = [
            Some(s.name.as_str()),
            s.branch.as_deref(),
            workspace.as_deref(),
            machine.map(MachineId::as_str),
        ];
        if let Some(rank) = best(&fields) {
            ranked.push((rank, FinderItem::Session(s.id.clone())));
        }
    }
    if let Some(Orphans::Listed(orphans)) = orphans {
        for orphan in orphans {
            let dir = dir_name(&orphan.path);
            if let Some(rank) = best(&[orphan.branch.as_deref(), dir.as_deref()]) {
                ranked.push((rank, FinderItem::Worktree(orphan.clone())));
            }
        }
    }
    // Stable: equal ranks keep the list's order.
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, item)| item).collect()
}

/// The last component of `path`, as text.
fn dir_name(path: &Path) -> Option<Cow<'_, str>> {
    path.file_name().map(|name| name.to_string_lossy())
}
