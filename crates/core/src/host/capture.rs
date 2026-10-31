//! What the host archives, and when (T42, decision 0020): a snapshot of an agent session's transcript when its agent
//! ends a turn (its `Stop` hook), when the session closes, and before any worktree it ran in is removed.
//!
//! Core decides which sessions to capture ([`CaptureJob`]s, in a [`HostCommand::Capture`](super::HostCommand::Capture));
//! the runtime runs them off its thread, asking the `Worktrees` port for the repo's root commit and the session's
//! commits since its base, then the `Archive` port. [`CaptureJob::requests`] turns a job and what the world said into
//! the archive's calls. A removal waits for its capture's answer ([`HostEvent::Captured`](super::HostEvent::Captured)):
//! a capture that fails keeps the worktree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::acpp::Timestamp;
use crate::config::AgentKind;
use crate::host::Remover;
use crate::ids::{MachineId, SessionId};
use crate::ports::archive::{ArchivedSession, CaptureReason, CaptureRequest};
use crate::ports::worktrees::{BranchName, CommitHash, PatchId, RepoRoot, WorktreePath};

/// One agent session's transcript to snapshot, with everything core knows about its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureJob {
    pub session: SessionId,
    pub reason: CaptureReason,
    /// Claude Code or Codex: no other kind has a transcript.
    pub agent: AgentKind,
    /// The agent's own id for its current session.
    pub agent_session_id: String,
    /// Where its hooks said that session's transcript is, as given.
    pub transcript_path: PathBuf,
    pub name: String,
    /// The machine its `session/created` was stamped with, as recorded.
    pub machine: MachineId,
    /// The `ts` of its `session/created`.
    pub created: Timestamp,
    /// Where its agent runs: its worktree, or the directory it was started in. The repo and its commits are found
    /// there.
    pub cwd: WorktreePath,
    pub branch: Option<BranchName>,
    /// The commit its worktree's branch started at, if recorded: its commits are those since.
    pub base: Option<CommitHash>,
    /// The agent profile it was started with, by name, if recorded: the runtime reads its env for the agent's home
    /// ([`AgentHomes::launched_with`]).
    pub profile: Option<String>,
}

/// What the world says about a [`CaptureJob`]'s repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub repo: RepoRoot,
    /// The session's commits since its base, oldest first: none without a base.
    pub commits: Vec<(CommitHash, Option<PatchId>)>,
}

impl CaptureJob {
    /// The session record and the snapshot to take: captured `at`, on machine `this` (which an entry from before
    /// machine ids names `local`), reading the transcript under `agent_home`. The head is the session's latest commit,
    /// or unknown when it has none of its own.
    pub fn requests(
        &self,
        found: Found,
        at: jiff::Timestamp,
        this: &MachineId,
        agent_home: PathBuf,
    ) -> (ArchivedSession, CaptureRequest) {
        let session = ArchivedSession {
            id: self.session.clone(),
            repo: found.repo,
            machine: self.machine.read_by(this),
            agent: self.agent,
            agent_session_id: self.agent_session_id.clone(),
            name: Some(self.name.clone()),
            branch: self.branch.clone(),
            base: self.base.clone(),
            cwd: self.cwd.as_path().to_path_buf(),
            created_at: self.created.instant(),
        };
        let request = CaptureRequest {
            session: self.session.clone(),
            at,
            head: found.commits.last().map(|(commit, _)| commit.clone()),
            reason: self.reason,
            commits: found.commits,
            agent_home,
            transcript_path: self.transcript_path.clone(),
        };
        (session, request)
    }
}

/// Claude Code's config dir variable.
pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
/// Codex's home variable.
pub const CODEX_HOME: &str = "CODEX_HOME";

/// Where each agent keeps its transcripts, resolved once by the host at start (core never reads the environment):
/// Claude Code's config dir (`CLAUDE_CONFIG_DIR`, or `~/.claude`) and Codex's `CODEX_HOME` (or `~/.codex`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHomes {
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl AgentHomes {
    /// The agents' default homes under the user's home directory `home`.
    pub fn under(home: &std::path::Path) -> Self {
        Self {
            claude: home.join(".claude"),
            codex: home.join(".codex"),
        }
    }

    /// The agents' homes for a user whose home directory is `home`, given the values of `CLAUDE_CONFIG_DIR` and
    /// `CODEX_HOME` (which the caller reads): each one set and not empty wins, else the default under `home`.
    pub fn resolve(
        home: &std::path::Path,
        claude_config_dir: Option<&std::ffi::OsStr>,
        codex_home: Option<&std::ffi::OsStr>,
    ) -> Self {
        let set = |v: Option<&std::ffi::OsStr>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
        let defaults = Self::under(home);
        Self {
            claude: set(claude_config_dir).unwrap_or(defaults.claude),
            codex: set(codex_home).unwrap_or(defaults.codex),
        }
    }

    /// `agent`'s home: `None` for a kind that keeps no transcript.
    pub fn of(&self, agent: AgentKind) -> Option<&std::path::Path> {
        match agent {
            AgentKind::ClaudeCode => Some(&self.claude),
            AgentKind::Codex => Some(&self.codex),
            AgentKind::Shell => None,
        }
    }

    /// The home of an `agent` launched with `env` added to the host's environment (its profile's env) in `cwd`: where
    /// it writes its transcripts. The agent's own variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) if `env` sets it, else
    /// the default under `env`'s `HOME` if it sets that (both agents find the user's home through `$HOME`), else this,
    /// the host's. Empty counts as unset, as in [`AgentHomes::resolve`]; a relative value is relative to `cwd`, as the
    /// agent, started there, reads it. `None` for a kind that keeps no transcript.
    pub fn launched_with(
        &self,
        agent: AgentKind,
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Option<PathBuf> {
        let var = match agent {
            AgentKind::ClaudeCode => CLAUDE_CONFIG_DIR,
            AgentKind::Codex => CODEX_HOME,
            AgentKind::Shell => return None,
        };
        let set = |name: &str| env.get(name).filter(|v| !v.is_empty()).map(|v| cwd.join(v));
        let home = match (set(var), set("HOME")) {
            (Some(own), _) => own,
            (None, Some(home)) => Self::under(&home).of(agent)?.to_path_buf(),
            (None, None) => self.of(agent)?.to_path_buf(),
        };
        Some(home)
    }
}

/// What follows a [`HostCommand::Capture`](super::HostCommand::Capture) once it has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterCapture {
    /// Nothing: the snapshot is all it was for.
    Nothing,
    /// Remove the worktree at `path` (`Worktrees::remove`, forced past uncommitted changes with `force`) for `remover`,
    /// if every capture succeeded; otherwise keep it, saying why.
    Remove {
        remover: Remover,
        path: WorktreePath,
        force: bool,
    },
}
