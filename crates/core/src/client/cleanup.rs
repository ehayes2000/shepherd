//! The cleanup view's rows (T25b): what each orphan worktree or stale session would lose if removed, and how removing
//! it went. [`Client`](super::Client) keeps them; the TUI draws them.

use std::collections::HashMap;

use crate::acpp::RequestId;
use crate::ids::SessionId;
use crate::ports::worktrees::WorktreeStatus;

use super::Orphan;

/// What a row of the cleanup view removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupItem {
    /// A managed worktree of the current workspace that no open session uses.
    Orphan(Orphan),
    /// A stale session: exited at least the host's `cleanup_after_days` ago, its branch merged. Removing it closes it,
    /// removing its worktree if that is still safe. Its name and branch as they were when it was listed.
    Session {
        id: SessionId,
        name: String,
        branch: Option<String>,
    },
}

/// What removing a row's worktree would lose, as far as the client knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupCheck {
    /// The host has been asked.
    Checking,
    Checked(WorktreeStatus),
    /// The host couldn't tell, for this reason, for the user.
    Failed(String),
}

/// How removing a row is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Waiting for the removals before it.
    Queued,
    /// The host is removing it.
    Removing,
    Removed,
    /// It was already gone.
    Gone,
    /// It was kept, for this reason, for the user.
    Kept(String),
}

/// One row of the cleanup view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRow {
    pub item: CleanupItem,
    pub check: CleanupCheck,
    /// `None` until the user asks to remove it.
    pub outcome: Option<CleanupOutcome>,
}

impl CleanupRow {
    /// It can be removed in bulk: removing it loses nothing (clean, and merged or pushed: T25's rule), and it hasn't
    /// been asked to be removed yet.
    pub fn safe(&self) -> bool {
        let loses_nothing = match &self.check {
            CleanupCheck::Checked(status) => status.losses().is_empty(),
            CleanupCheck::Checking | CleanupCheck::Failed(_) => false,
        };
        loses_nothing && self.outcome.is_none()
    }
}

/// Where listing the cleanup view's rows is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupListing {
    /// The host has been asked for the orphans.
    Loading,
    /// The orphans are listed (stale sessions may still be checked, row by row).
    Listed,
    /// They couldn't be listed, for this reason, for the user.
    Failed(String),
}

/// The removal under way, and what ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Removal {
    /// An orphan's `_shepherd/worktree/remove`: its answer.
    Orphan(RequestId),
    /// A stale session's close: its `session/closed`, or a refusal of `request` (until it is acked).
    Session {
        id: SessionId,
        request: Option<RequestId>,
    },
}

/// The cleanup view's state. Its rows and removals outlive the popup: closing the view cancels nothing.
#[derive(Debug, Clone, Default)]
pub(super) struct Cleanup {
    pub rows: Vec<CleanupRow>,
    pub listing: Option<CleanupListing>,
    /// The unanswered request for the orphans.
    pub list_request: Option<RequestId>,
    /// Unanswered status requests for stale sessions.
    pub checks: HashMap<RequestId, SessionId>,
    pub running: Option<Removal>,
    /// The index of the first row the view shows.
    pub scroll: usize,
}

impl Cleanup {
    /// Removals are queued or under way.
    pub fn busy(&self) -> bool {
        self.rows.iter().any(|r| {
            matches!(
                r.outcome,
                Some(CleanupOutcome::Queued | CleanupOutcome::Removing)
            )
        })
    }

    /// The row being removed.
    pub fn removing(&mut self) -> Option<&mut CleanupRow> {
        self.rows
            .iter_mut()
            .find(|r| r.outcome == Some(CleanupOutcome::Removing))
    }

    /// Ends the removal under way with `outcome`.
    pub fn finish(&mut self, outcome: CleanupOutcome) {
        self.running = None;
        if let Some(row) = self.removing() {
            row.outcome = Some(outcome);
        }
    }
}
