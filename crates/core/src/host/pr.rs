//! Polling each session's pull request (T27).
//!
//! A session is polled once its worktree exists (`worktree/created`, which names its branch) and until it closes or
//! starts closing (`worktree/removing`). Its first check is due [`POLL_INTERVAL`] after its worktree was made (a new
//! branch has no pull request yet), and each later one [`POLL_INTERVAL`] after the call before it, so a session is
//! asked about at most once per interval. Only one check per session runs at a time. A check that fails backs off:
//! the interval doubles with each failure in a row, up to [`MAX_BACKOFF`], and an answer resets it.
//!
//! An answer that differs from the pull request last recorded is appended as `_shepherd/session/pr`, so the host and
//! every client fold it like any other fact. An unavailable forge (`gh` missing or not signed in) counts as no pull
//! request, shown as nothing; any other failure keeps what was last recorded.
//!
//! Not host state: when the last call happened is known only to this host run. The runtime keeps one [`PrWatch`],
//! applies every entry it appends (and replays the logs into it on start, so a restarted host checks every session
//! at once and knows the pull requests it recorded), feeds it ticks stamped with the clock, wakes up at its
//! [`PrWatch::deadline`], runs the [`HostCommand::CheckPr`]s it returns on a worker, and feeds back each answer.

use std::collections::{BTreeMap, BTreeSet};

use jiff::SignedDuration;

use super::{HostCommand, append};
use crate::acpp::{Envelope, Frame, Pr, SessionPr, ShepherdFrame, Timestamp};
use crate::ids::SessionId;
use crate::ports::forge::ForgeError;
use crate::ports::worktrees::{BranchName, RepoRef};

/// How long after a session's last check (or its worktree's creation) the next one is due.
pub const POLL_INTERVAL: SignedDuration = SignedDuration::from_secs(60);
/// The longest wait between checks of a session whose checks keep failing.
pub const MAX_BACKOFF: SignedDuration = SignedDuration::from_secs(16 * 60);

/// Time passing, and the forge's answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrEvent {
    /// It is `at`: the deadline the runtime scheduled, or later.
    Tick { at: Timestamp },
    /// The answer to a [`HostCommand::CheckPr`] for `session`.
    Checked {
        session: SessionId,
        result: Result<Option<Pr>, ForgeError>,
    },
}

/// Polls open sessions' pull requests. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrWatch {
    sessions: BTreeMap<SessionId, Polled>,
    /// Sessions that closed or started closing: never polled again, even if a worktree is recorded for them later.
    done: BTreeSet<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Polled {
    repo: RepoRef,
    branch: BranchName,
    /// The pull request last recorded.
    shown: Option<Pr>,
    /// When its worktree was made, or its last check was asked for.
    last: jiff::Timestamp,
    /// Checks that failed in a row.
    failures: u32,
    /// A check is running.
    checking: bool,
}

impl Polled {
    fn due(&self) -> Option<jiff::Timestamp> {
        if self.checking {
            return None;
        }
        let factor = 1i32 << self.failures.min(4);
        let wait = (POLL_INTERVAL * factor).min(MAX_BACKOFF);
        self.last.checked_add(wait).ok()
    }
}

impl PrWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one log entry: `worktree/created` starts polling its session, `worktree/removing` and `session/closed`
    /// stop it, and `session/pr` is what was last recorded.
    pub fn apply(&mut self, env: &Envelope) {
        let (Some(id), Frame::Shepherd(m)) = (env.session(), env.frame()) else {
            return;
        };
        match m.body() {
            ShepherdFrame::WorktreeCreated(worktree) if !self.done.contains(id) => {
                self.sessions.entry(id.clone()).or_insert(Polled {
                    repo: RepoRef::new(&worktree.path),
                    branch: BranchName::new(&worktree.branch),
                    shown: None,
                    last: env.ts().instant(),
                    failures: 0,
                    checking: false,
                });
            }
            ShepherdFrame::WorktreeRemoving(_) | ShepherdFrame::SessionClosed(_) => {
                self.sessions.remove(id);
                self.done.insert(id.clone());
            }
            ShepherdFrame::SessionPr(pr) => {
                if let Some(polled) = self.sessions.get_mut(id) {
                    polled.shown = pr.pr.clone();
                }
            }
            _ => {}
        }
    }

    /// Asks for a check of every session that is due, or appends what an answer changed.
    pub fn handle(&mut self, event: PrEvent) -> Vec<HostCommand> {
        match event {
            PrEvent::Tick { at } => {
                let now = at.instant();
                self.sessions
                    .iter_mut()
                    .filter(|(_, polled)| polled.due().is_some_and(|due| due <= now))
                    .map(|(id, polled)| {
                        polled.checking = true;
                        polled.last = now;
                        HostCommand::CheckPr {
                            session: id.clone(),
                            repo: polled.repo.clone(),
                            branch: polled.branch.clone(),
                        }
                    })
                    .collect()
            }
            PrEvent::Checked { session, result } => {
                let Some(polled) = self.sessions.get_mut(&session) else {
                    return Vec::new();
                };
                polled.checking = false;
                let pr = match result {
                    Ok(pr) => {
                        polled.failures = 0;
                        pr
                    }
                    Err(error) => {
                        polled.failures = polled.failures.saturating_add(1);
                        match error {
                            ForgeError::Unavailable(_) => None,
                            ForgeError::Timeout | ForgeError::Other(_) => return Vec::new(),
                        }
                    }
                };
                if pr == polled.shown {
                    return Vec::new();
                }
                vec![append(&session, ShepherdFrame::SessionPr(SessionPr { pr }))]
            }
        }
    }

    /// The earliest time a session's next check is due, if any: when to send the next [`PrEvent::Tick`].
    pub fn deadline(&self) -> Option<jiff::Timestamp> {
        self.sessions.values().filter_map(Polled::due).min()
    }
}
