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
    AgentStarted(AgentStarted),
    AgentIdle(AgentIdle),
    AgentExited(AgentExited),
    WorktreeCreated(WorktreeCreated),
    WorktreeHook(WorktreeHook),
    WorkspaceSelected(WorkspaceSelected),
}

impl ShepherdFrame {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionCreated(_) => "_shepherd/session/created",
            Self::SessionRenamed(_) => "_shepherd/session/renamed",
            Self::SessionClosed(_) => "_shepherd/session/closed",
            Self::SessionFailed(_) => "_shepherd/session/failed",
            Self::AgentStarted(_) => "_shepherd/agent/started",
            Self::AgentIdle(_) => "_shepherd/agent/idle",
            Self::AgentExited(_) => "_shepherd/agent/exited",
            Self::WorktreeCreated(_) => "_shepherd/worktree/created",
            Self::WorktreeHook(_) => "_shepherd/worktree/hook",
            Self::WorkspaceSelected(_) => "_shepherd/workspace/selected",
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
            Self::AgentStarted(p) => to_params(p),
            Self::AgentIdle(p) => to_params(p),
            Self::AgentExited(p) => to_params(p),
            Self::WorktreeCreated(p) => to_params(p),
            Self::WorktreeHook(p) => to_params(p),
            Self::WorkspaceSelected(p) => to_params(p),
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
            "_shepherd/agent/started" => Self::AgentStarted(p(params)?),
            "_shepherd/agent/idle" => Self::AgentIdle(p(params)?),
            "_shepherd/agent/exited" => Self::AgentExited(p(params)?),
            "_shepherd/worktree/created" => Self::WorktreeCreated(p(params)?),
            "_shepherd/worktree/hook" => Self::WorktreeHook(p(params)?),
            "_shepherd/workspace/selected" => Self::WorkspaceSelected(p(params)?),
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
}

/// The session couldn't start: its PTY never spawned, e.g. because its worktree couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFailed {
    /// Why, for the user.
    pub reason: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeHook {
    pub hook: String,
    /// `None` when the hook didn't exit normally (killed by a signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSelected {
    pub repo: String,
}
