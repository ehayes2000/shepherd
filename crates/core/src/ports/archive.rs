//! The conversation archive (decision 0020): a copy of every agent conversation, kept on this device and indexed by
//! repo and commit, so removing a worktree never loses what its agent knew.
//!
//! The real archive is the independent `shepherd-archive` crate, which core can't see: these are core's own types, and
//! the adapter in the `shepherd` crate converts them. The trait is synchronous and its calls block (they read
//! transcripts and write an index): the host calls it on a worker thread, never its runtime thread.

use std::fmt;
use std::path::PathBuf;

use crate::config::AgentKind;
use crate::ids::{MachineId, SessionId};
use crate::ports::worktrees::{BranchName, CommitHash, PatchId, RepoRoot};

/// Why a snapshot is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureReason {
    /// The agent finished a turn (its `Stop` hook).
    TurnEnd,
    /// The session was closed.
    Close,
    /// Its worktree is about to be removed.
    BeforeRemove,
    /// Someone asked for it.
    Manual,
}

impl fmt::Display for CaptureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TurnEnd => "turn end",
            Self::Close => "close",
            Self::BeforeRemove => "before remove",
            Self::Manual => "manual",
        })
    }
}

/// The session that captures belong to, as the archive records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSession {
    pub id: SessionId,
    /// The repo it ran in.
    pub repo: RepoRoot,
    pub machine: MachineId,
    pub agent: AgentKind,
    /// The agent's own id for its session (what its resume flag takes). Captures read that session's transcript.
    pub agent_session_id: String,
    pub name: Option<String>,
    pub branch: Option<BranchName>,
    /// The commit its worktree's branch started at, if known.
    pub base: Option<CommitHash>,
    pub cwd: PathBuf,
    pub created_at: jiff::Timestamp,
}

/// One snapshot of a recorded session's transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRequest {
    pub session: SessionId,
    pub at: jiff::Timestamp,
    /// The commit its worktree was at, if known.
    pub head: Option<CommitHash>,
    pub reason: CaptureReason,
    /// The session's commits so far (`base..HEAD`), each with its patch-id.
    pub commits: Vec<(CommitHash, Option<PatchId>)>,
    /// The agent's home: Claude Code's config dir (`CLAUDE_CONFIG_DIR`, or `~/.claude`), or Codex's `CODEX_HOME` (or
    /// `~/.codex`). Resolved by the host at start.
    pub agent_home: PathBuf,
    /// The `transcript_path` the agent's hooks gave, as given: never one shepherd computed.
    pub transcript_path: PathBuf,
}

/// Why an archive call stored nothing. Its text is for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    /// The archive refused what it was given: a transcript path outside the agent's own place for that session, past
    /// its limits, or a value it can't record (e.g. an agent kind it doesn't archive).
    #[error("{0}")]
    Refused(String),
    /// The transcript doesn't exist (the agent named one it never wrote, or deleted it since): there is nothing to
    /// save, and nothing was stored.
    #[error("{0}")]
    Missing(String),
    /// Something went wrong storing it: reading a file, writing the index, a session it doesn't know.
    #[error("{0}")]
    Failed(String),
}

pub trait Archive {
    /// Records (or updates) the session that captures belong to. Idempotent.
    fn put_session(&self, session: &ArchivedSession) -> Result<(), ArchiveError>;
    /// Snapshots the session's transcripts. Blocking: the host calls it on its worker, never the runtime thread. A
    /// session [`Archive::put_session`] never recorded is [`ArchiveError::Failed`].
    fn capture(&self, capture: &CaptureRequest) -> Result<(), ArchiveError>;
}
