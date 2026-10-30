//! An in-memory `Forge`: pull requests by head branch, in the order they were opened, and a switch for how the forge
//! behaves (available, its tool missing or signed out, failing, never answering). It ignores which checkout of a repo
//! it is asked about. A forge that never answers answers [`ForgeError::Timeout`] at once, as the real port does once
//! its bound has passed.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another. Every call is recorded
//! ([`FakeForge::calls`]), so scenarios can count them.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd_core::ports::forge::{Forge, ForgeError, Pr, PrState};
use shepherd_core::ports::{BranchName, RepoRef};

/// How the fake forge answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Behavior {
    #[default]
    Available,
    /// Its tool isn't installed.
    Missing,
    /// Its tool isn't signed in.
    SignedOut,
    /// Every call fails with this.
    Failing(String),
    /// It never answers.
    Hanging,
}

#[derive(Debug, Default)]
struct State {
    /// Head branch and pull request, oldest first.
    prs: Vec<(String, Pr)>,
    behavior: Behavior,
    calls: Vec<(RepoRef, BranchName)>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeForge {
    state: Arc<Mutex<State>>,
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens `pr` from `branch`, newer than every pull request opened before.
    pub fn open_pr(&self, branch: &str, pr: Pr) {
        self.state().prs.push((branch.to_owned(), pr));
    }

    /// Replaces pull request `number` (from any branch) with `pr`, e.g. its checks finished.
    pub fn update_pr(&self, number: u64, pr: Pr) {
        for (_, existing) in self.state().prs.iter_mut() {
            if existing.number == number {
                *existing = pr.clone();
            }
        }
    }

    pub fn set_behavior(&self, behavior: Behavior) {
        self.state().behavior = behavior;
    }

    /// Every call made, in order: the repo and branch asked about.
    pub fn calls(&self) -> Vec<(RepoRef, BranchName)> {
        self.state().calls.clone()
    }
}

impl Forge for FakeForge {
    fn pr_for_branch(&self, repo: &RepoRef, branch: &BranchName) -> Result<Option<Pr>, ForgeError> {
        let mut state = self.state();
        state.calls.push((repo.clone(), branch.clone()));
        match &state.behavior {
            Behavior::Available => {}
            Behavior::Missing => return Err(ForgeError::Unavailable("gh is not installed".into())),
            Behavior::SignedOut => {
                return Err(ForgeError::Unavailable("gh is not signed in".into()));
            }
            Behavior::Failing(why) => return Err(ForgeError::Other(why.clone())),
            Behavior::Hanging => return Err(ForgeError::Timeout),
        }
        let mut newest_first = state
            .prs
            .iter()
            .rev()
            .filter(|(head, _)| head == branch.as_str())
            .map(|(_, pr)| pr);
        let newest = newest_first.clone().next();
        Ok(newest_first
            .find(|pr| pr.state == PrState::Open)
            .or(newest)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use shepherd_core::ports::forge::{Check, PrState, rollup};
    use shepherd_core::ports::{Pr, RepoRef};

    use super::{Behavior, FakeForge};
    use crate::contracts::forge::ForgeFixture;

    pub struct FakeFixture(FakeForge);

    impl FakeFixture {
        pub fn new() -> Self {
            Self(FakeForge::new())
        }
    }

    impl ForgeFixture for FakeFixture {
        type Port = FakeForge;

        fn port(&self) -> &FakeForge {
            &self.0
        }

        fn repo(&self) -> RepoRef {
            RepoRef::new("/src/app")
        }

        fn add_pr(&mut self, branch: &str, number: u64, state: PrState, checks: &[Check]) {
            let checks = rollup(checks.iter().copied());
            self.0.open_pr(
                branch,
                Pr {
                    number,
                    state,
                    checks,
                },
            );
        }

        fn uninstall(&mut self) {
            self.0.set_behavior(Behavior::Missing);
        }

        fn sign_out(&mut self) {
            self.0.set_behavior(Behavior::SignedOut);
        }

        fn break_forge(&mut self) {
            self.0.set_behavior(Behavior::Failing("HTTP 502".into()));
        }

        fn hang(&mut self) {
            self.0.set_behavior(Behavior::Hanging);
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::forge_contract!(FakeFixture::new());
    }
}
