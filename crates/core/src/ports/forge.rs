//! The forge a repo's pull requests live on (GitHub, through `gh`), asked which pull request a branch has.
//!
//! The trait is synchronous and every call is bounded: it waits on the network, so the runtime calls it on a worker
//! thread, never on its loop. No call waits on a person: one that would need to prompt (to sign in, say) fails.
//!
//! A branch's pull request is the open one with that head branch, if there is one, else the newest. What the forge
//! can't tell us at all, because its tool is missing or not signed in, is [`ForgeError::Unavailable`]: the host shows
//! that as no pull request, never as an error.

use super::worktrees::{BranchName, RepoRef};
pub use crate::acpp::{Pr, PrChecks, PrState};

/// One check on a pull request's head commit, as far as rolling them up goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// It finished and didn't fail: a success, or a neutral or skipped result.
    Passed,
    /// It finished and failed, errored, was cancelled or timed out, or needs action.
    Failed,
    /// It hasn't finished, or hasn't started.
    Pending,
}

/// How `checks` stand together: failing if any failed, else pending if any hasn't finished, else passing; none if
/// there are none.
pub fn rollup(checks: impl IntoIterator<Item = Check>) -> PrChecks {
    let (mut any, mut pending) = (false, false);
    for check in checks {
        any = true;
        match check {
            Check::Failed => return PrChecks::Failing,
            Check::Pending => pending = true,
            Check::Passed => {}
        }
    }
    match (any, pending) {
        (false, _) => PrChecks::None,
        (true, true) => PrChecks::Pending,
        (true, false) => PrChecks::Passing,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForgeError {
    /// The forge can't be asked at all: its tool is missing, or not signed in. Not an error to show.
    #[error("the forge is unavailable: {0}")]
    Unavailable(String),
    /// It didn't answer within the call's bound.
    #[error("the forge didn't answer in time")]
    Timeout,
    /// Anything else, e.g. the repo has no GitHub remote, or the forge answered something unreadable.
    #[error("{0}")]
    Other(String),
}

pub trait Forge {
    /// The pull request whose head is `branch` in `repo` (any checkout of it), or `None` if it has none.
    fn pr_for_branch(&self, repo: &RepoRef, branch: &BranchName) -> Result<Option<Pr>, ForgeError>;
}
