//! The `_shepherd/*` extension frames: facts ACP has no message for. See the table in `docs/acpp.md`.
//!
//! Params use snake_case field names. Fields a producer may not know yet (a plain shell session has no worktree
//! or agent) are optional.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Frame;
use crate::ids::MachineId;

/// One frame from the initial `_shepherd/*` table, by method.
#[derive(Debug, Clone, PartialEq)]
pub enum ShepherdFrame {
    SessionCreated(SessionCreated),
    SessionRenamed(SessionRenamed),
    SessionClosed(SessionClosed),
    SessionFailed(SessionFailed),
    SessionPr(SessionPr),
    AgentStarted(AgentStarted),
    AgentIdle(AgentIdle),
    AgentExited(AgentExited),
    WorktreeCreated(WorktreeCreated),
    WorktreeHook(WorktreeHook),
    WorktreeRemoving(WorktreeRemoving),
    WorktreeRemoved(WorktreeRemoved),
    WorkspaceSelected(WorkspaceSelected),
    MachineRenamed(MachineRenamed),
    PeerTrusted(PeerTrusted),
    PeerRemoved(PeerRemoved),
}

impl ShepherdFrame {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionCreated(_) => "_shepherd/session/created",
            Self::SessionRenamed(_) => "_shepherd/session/renamed",
            Self::SessionClosed(_) => "_shepherd/session/closed",
            Self::SessionFailed(_) => "_shepherd/session/failed",
            Self::SessionPr(_) => "_shepherd/session/pr",
            Self::AgentStarted(_) => "_shepherd/agent/started",
            Self::AgentIdle(_) => "_shepherd/agent/idle",
            Self::AgentExited(_) => "_shepherd/agent/exited",
            Self::WorktreeCreated(_) => "_shepherd/worktree/created",
            Self::WorktreeHook(_) => "_shepherd/worktree/hook",
            Self::WorktreeRemoving(_) => "_shepherd/worktree/removing",
            Self::WorktreeRemoved(_) => "_shepherd/worktree/removed",
            Self::WorkspaceSelected(_) => "_shepherd/workspace/selected",
            Self::MachineRenamed(_) => "_shepherd/machine/renamed",
            Self::PeerTrusted(_) => "_shepherd/peer/trusted",
            Self::PeerRemoved(_) => "_shepherd/peer/removed",
        }
    }

    /// Builds the notification for this frame. It parses back to an equal `ShepherdFrame`, except that an
    /// `AgentStarted::command` of `Some(Value::Null)` reads back as `None`.
    pub fn to_frame(&self) -> Frame {
        let params = match self {
            Self::SessionCreated(p) => to_params(p),
            Self::SessionRenamed(p) => to_params(p),
            Self::SessionClosed(p) => to_params(p),
            Self::SessionFailed(p) => to_params(p),
            Self::SessionPr(p) => to_params(p),
            Self::AgentStarted(p) => to_params(p),
            Self::AgentIdle(p) => to_params(p),
            Self::AgentExited(p) => to_params(p),
            Self::WorktreeCreated(p) => to_params(p),
            Self::WorktreeHook(p) => to_params(p),
            Self::WorktreeRemoving(p) => to_params(p),
            Self::WorktreeRemoved(p) => to_params(p),
            Self::WorkspaceSelected(p) => to_params(p),
            Self::MachineRenamed(p) => to_params(p),
            Self::PeerTrusted(p) => to_params(p),
            Self::PeerRemoved(p) => to_params(p),
        };
        Frame::notification(self.method(), params)
    }

    /// Parses the params of a `_shepherd/*` method. `None` for a method outside the table or params of the wrong
    /// shape.
    pub(crate) fn parse(method: &str, params: Value) -> Option<Self> {
        fn p<T: DeserializeOwned>(params: Value) -> Option<T> {
            serde_json::from_value(params).ok()
        }
        Some(match method {
            "_shepherd/session/created" => Self::SessionCreated(p(params)?),
            "_shepherd/session/renamed" => Self::SessionRenamed(p(params)?),
            "_shepherd/session/closed" => Self::SessionClosed(p(params)?),
            "_shepherd/session/failed" => Self::SessionFailed(p(params)?),
            "_shepherd/session/pr" => Self::SessionPr(p(params)?),
            "_shepherd/agent/started" => Self::AgentStarted(p(params)?),
            "_shepherd/agent/idle" => Self::AgentIdle(p(params)?),
            "_shepherd/agent/exited" => Self::AgentExited(p(params)?),
            "_shepherd/worktree/created" => Self::WorktreeCreated(p(params)?),
            "_shepherd/worktree/hook" => Self::WorktreeHook(p(params)?),
            "_shepherd/worktree/removing" => Self::WorktreeRemoving(p(params)?),
            "_shepherd/worktree/removed" => Self::WorktreeRemoved(p(params)?),
            "_shepherd/workspace/selected" => Self::WorkspaceSelected(p(params)?),
            "_shepherd/machine/renamed" => Self::MachineRenamed(p(params)?),
            "_shepherd/peer/trusted" => Self::PeerTrusted(p(params)?),
            "_shepherd/peer/removed" => Self::PeerRemoved(p(params)?),
            _ => return None,
        })
    }
}

/// Serializes a frame's params. Params hold only strings, numbers, bools and already-parsed JSON, which always
/// serialize.
pub(super) fn to_params<T: Serialize>(p: &T) -> Value {
    serde_json::to_value(p)
        .unwrap_or_else(|e| unreachable!("frame params failed to serialize: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineId>,
    /// The agent profile the session was started with, by name. Resume runs it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The directory the agent was started in. Resume starts it there again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamed {
    pub name: String,
    /// True when shepherd picked the name, false when the user did.
    #[serde(default)]
    pub auto: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosed {
    #[serde(default)]
    pub worktree_removed: bool,
    /// Why the session's worktree was kept when closing tried to remove it, for the user. Absent when it was removed,
    /// was already gone, or the user chose to keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_kept: Option<String>,
}

/// The session couldn't start: its PTY never spawned, e.g. because its worktree couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFailed {
    /// Why, for the user.
    pub reason: String,
}

/// The session's branch has a different pull request than last recorded, or none any more. The latest one is current.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPr {
    /// `None`: the branch has no pull request, or none can be found out (e.g. no `gh`, or not signed in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<Pr>,
}

/// A pull request for a session's branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub state: PrState,
    pub checks: PrChecks,
}

/// Where a pull request is in its life. Open: a value this build doesn't know is kept as [`PrState::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrState {
    Open,
    /// Closed without being merged.
    Closed,
    Merged,
    #[serde(untagged)]
    Other(String),
}

/// How a pull request's checks stand, taken together. Open: a value this build doesn't know is kept as
/// [`PrChecks::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrChecks {
    /// It has no checks.
    None,
    /// Some haven't finished, and none has failed.
    Pending,
    /// Every one finished, and none failed.
    Passing,
    /// At least one failed.
    Failing,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStarted {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The command line, as the producer recorded it (a string or an argv array).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Value>,
    /// Where the agent writes this agent session's transcript, as its hook payload's `transcript_path` said. Data
    /// only: core never resolves or reads it. The conversation archive captures from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// The agent is at its prompt with nothing to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdle {
    /// Set when no agent signal said so and the host inferred it. `None` for a real idle (e.g. a `Stop` hook).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred: Option<IdleInference>,
}

/// How the host inferred an idle. Open: a value this build doesn't know is kept as [`IdleInference::Other`], so the
/// frame is still an idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdleInference {
    /// The PTY went quiet during a turn.
    Quiet,
    /// The PTY redrew after a permission request (the dialog was answered), then went quiet.
    QuietAfterPermission,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExited {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCreated {
    pub path: String,
    pub branch: String,
    /// What the branch started from: `origin/<default>`, or the local `<default>` when origin had none.
    pub base: String,
    /// Set when fetching origin failed, so `base` is as of the last fetch (or local) and may be behind origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
    /// The workspace's post-create hooks, run in order in the worktree before the session's PTY spawns. Each one's
    /// result is a `worktree/hook`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_create: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeHook {
    pub hook: String,
    /// `None` when the hook didn't exit normally (killed by a signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output_tail: String,
    /// Why it didn't exit normally, or couldn't run (a timeout, a signal, a missing directory), for the user. Absent
    /// when it exited with `exit_code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The worktree it ran in. Set on the pre-remove hooks of orphan worktrees, whose records share the host log;
    /// absent on a session's own hooks, whose log says whose worktree it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Closing the session is removing its worktree: the session stays listed, its PTY gone, until its `session/closed`
/// says how that went. In the host log (no session): the cleanup view is removing an orphan worktree, one no open
/// session uses, until a `worktree/removed` for the same path says how that went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoving {
    pub path: String,
    /// Removed even if that loses uncommitted changes or unpushed commits (the user said so).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// The workspace's pre-remove hooks, run in order in the worktree before it is removed. Each one's result is a
    /// `worktree/hook`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_remove: Vec<String>,
}

/// Host log only: how removing the orphan worktree at `path` (a `worktree/removing` in the host log) went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoved {
    pub path: String,
    pub removed: bool,
    /// Why it was kept, for the user. Absent when it was removed, or was already gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSelected {
    pub repo: String,
}

/// The host started under a name other than the last one recorded (host log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRenamed {
    pub name: String,
}

/// This machine trusts machine `node` (an iroh node id, as [`MachineId`]s are), which calls itself `name`: they paired
/// (host log). A later one for the same machine replaces its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerTrusted {
    pub node: MachineId,
    pub name: String,
}

/// This machine no longer trusts machine `node` (host log): `shepherd unpair`. Pairing again trusts it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRemoved {
    pub node: MachineId,
}
