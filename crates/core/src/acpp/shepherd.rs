//! The `_shepherd/*` extension frames: facts ACP has no message for. See the table in `docs/acpp.md`.
//!
//! Params use snake_case field names. Fields a producer may not know yet (a plain shell session has no worktree
//! or agent) are optional.

use std::num::NonZeroU32;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Frame;
use crate::ids::{MachineId, SessionId};
use crate::layout::Layout;

/// One frame from the initial `_shepherd/*` table, by method.
#[derive(Debug, Clone, PartialEq)]
pub enum ShepherdFrame {
    SessionCreated(SessionCreated),
    SessionRenamed(SessionRenamed),
    SessionClosed(SessionClosed),
    SessionFailed(SessionFailed),
    SessionPr(SessionPr),
    SessionPrompted(SessionPrompted),
    SessionAnswered(SessionAnswered),
    AgentStarted(AgentStarted),
    AgentAsked(AgentAsked),
    AgentIdle(AgentIdle),
    AgentExited(AgentExited),
    SubagentStarted(SubagentStarted),
    SubagentBlocked(SubagentBlocked),
    SubagentWorking(SubagentWorking),
    SubagentFinished(SubagentFinished),
    WorktreeCreated(WorktreeCreated),
    WorktreeHook(WorktreeHook),
    WorktreeRemoving(WorktreeRemoving),
    WorktreeRemoved(WorktreeRemoved),
    WorkspaceSelected(WorkspaceSelected),
    MachineRenamed(MachineRenamed),
    PeerTrusted(PeerTrusted),
    PeerRemoved(PeerRemoved),
    UiLayout(UiLayout),
}

impl ShepherdFrame {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionCreated(_) => "_shepherd/session/created",
            Self::SessionRenamed(_) => "_shepherd/session/renamed",
            Self::SessionClosed(_) => "_shepherd/session/closed",
            Self::SessionFailed(_) => "_shepherd/session/failed",
            Self::SessionPr(_) => "_shepherd/session/pr",
            Self::SessionPrompted(_) => "_shepherd/session/prompted",
            Self::SessionAnswered(_) => "_shepherd/session/answered",
            Self::AgentStarted(_) => "_shepherd/agent/started",
            Self::AgentAsked(_) => "_shepherd/agent/asked",
            Self::AgentIdle(_) => "_shepherd/agent/idle",
            Self::AgentExited(_) => "_shepherd/agent/exited",
            Self::SubagentStarted(_) => "_shepherd/subagent/started",
            Self::SubagentBlocked(_) => "_shepherd/subagent/blocked",
            Self::SubagentWorking(_) => "_shepherd/subagent/working",
            Self::SubagentFinished(_) => "_shepherd/subagent/finished",
            Self::WorktreeCreated(_) => "_shepherd/worktree/created",
            Self::WorktreeHook(_) => "_shepherd/worktree/hook",
            Self::WorktreeRemoving(_) => "_shepherd/worktree/removing",
            Self::WorktreeRemoved(_) => "_shepherd/worktree/removed",
            Self::WorkspaceSelected(_) => "_shepherd/workspace/selected",
            Self::MachineRenamed(_) => "_shepherd/machine/renamed",
            Self::PeerTrusted(_) => "_shepherd/peer/trusted",
            Self::PeerRemoved(_) => "_shepherd/peer/removed",
            Self::UiLayout(_) => "_shepherd/ui/layout",
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
            Self::SessionPrompted(p) => to_params(p),
            Self::SessionAnswered(p) => to_params(p),
            Self::AgentStarted(p) => to_params(p),
            Self::AgentAsked(p) => to_params(p),
            Self::AgentIdle(p) => to_params(p),
            Self::AgentExited(p) => to_params(p),
            Self::SubagentStarted(p) => to_params(p),
            Self::SubagentBlocked(p) => to_params(p),
            Self::SubagentWorking(p) => to_params(p),
            Self::SubagentFinished(p) => to_params(p),
            Self::WorktreeCreated(p) => to_params(p),
            Self::WorktreeHook(p) => to_params(p),
            Self::WorktreeRemoving(p) => to_params(p),
            Self::WorktreeRemoved(p) => to_params(p),
            Self::WorkspaceSelected(p) => to_params(p),
            Self::MachineRenamed(p) => to_params(p),
            Self::PeerTrusted(p) => to_params(p),
            Self::PeerRemoved(p) => to_params(p),
            Self::UiLayout(p) => to_params(p),
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
            "_shepherd/session/prompted" => Self::SessionPrompted(p(params)?),
            "_shepherd/session/answered" => Self::SessionAnswered(p(params)?),
            "_shepherd/agent/started" => Self::AgentStarted(p(params)?),
            "_shepherd/agent/asked" => Self::AgentAsked(p(params)?),
            "_shepherd/agent/idle" => Self::AgentIdle(p(params)?),
            "_shepherd/agent/exited" => Self::AgentExited(p(params)?),
            "_shepherd/subagent/started" => Self::SubagentStarted(p(params)?),
            "_shepherd/subagent/blocked" => Self::SubagentBlocked(p(params)?),
            "_shepherd/subagent/working" => Self::SubagentWorking(p(params)?),
            "_shepherd/subagent/finished" => Self::SubagentFinished(p(params)?),
            "_shepherd/worktree/created" => Self::WorktreeCreated(p(params)?),
            "_shepherd/worktree/hook" => Self::WorktreeHook(p(params)?),
            "_shepherd/worktree/removing" => Self::WorktreeRemoving(p(params)?),
            "_shepherd/worktree/removed" => Self::WorktreeRemoved(p(params)?),
            "_shepherd/workspace/selected" => Self::WorkspaceSelected(p(params)?),
            "_shepherd/machine/renamed" => Self::MachineRenamed(p(params)?),
            "_shepherd/peer/trusted" => Self::PeerTrusted(p(params)?),
            "_shepherd/peer/removed" => Self::PeerRemoved(p(params)?),
            "_shepherd/ui/layout" => Self::UiLayout(p(params)?),
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
    /// Something else about how closing went, for the user: e.g. that no transcript was found to archive before the
    /// worktree was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

/// The host typed a prompt into the session's agent (`_shepherd/session/prompt`, decision 0022), so the UI can show the
/// last thing the agent was told and by whom. `text` is kept whole, however long; it is shortened only when drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPrompted {
    pub text: String,
    pub by: PromptBy,
}

/// Who had the host prompt an agent: `cli`, `client` (the TUI, or any client that didn't say), or `mcp:<session>` (an
/// agent running in that session, through shepherd's MCP server). Open: a value this build doesn't know is kept as
/// [`PromptBy::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum PromptBy {
    Cli,
    Client,
    Mcp(SessionId),
    Other(String),
}

const MCP_PREFIX: &str = "mcp:";

impl From<String> for PromptBy {
    fn from(text: String) -> Self {
        match text.as_str() {
            "cli" => Self::Cli,
            "client" => Self::Client,
            _ => match text.strip_prefix(MCP_PREFIX) {
                Some(session) if !session.is_empty() => Self::Mcp(SessionId::new(session)),
                _ => Self::Other(text),
            },
        }
    }
}

impl From<PromptBy> for String {
    fn from(by: PromptBy) -> Self {
        match by {
            PromptBy::Cli => "cli".to_owned(),
            PromptBy::Client => "client".to_owned(),
            PromptBy::Mcp(session) => format!("{MCP_PREFIX}{session}"),
            PromptBy::Other(text) => text,
        }
    }
}

impl std::fmt::Display for PromptBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from(self.clone()))
    }
}

/// The agent is blocked on a question to the user (T51): a permission dialog for a tool, or a question it asked. It
/// stays asked until it is answered (`session/answered`, or the agent's own activity moving on) or the turn ends.
/// Text is the agent's, raw: display it only through `display_safe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAsked {
    pub kind: BlockKind,
    /// What it asks, for the user: the tool and its main argument, or the question.
    pub text: String,
    /// A question's options, in the order the agent shows them (an answer's index counts from 1). Absent for a
    /// permission, and for a question that can't be answered by picking one option (several questions, or several
    /// picks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    /// The subagent that asked, by the agent's id for it, when a subagent did: its dialog is in the parent's TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    /// The tool it asks about, as the hook named it (`Bash`, `Edit`, `apply_patch`, `AskUserQuestion`, …): it says
    /// which of the agent's dialogs is up, and so which answers it offers. Absent when the hook didn't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

/// The agent's pending question was answered (T51): the host typed `choice` for it on behalf of `by`
/// (`_shepherd/session/answer`), or, without a `choice`, a client typed into the session while it was asked, so the
/// user is answering it in the pane and shepherd must not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAnswered {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<Choice>,
    pub by: PromptBy,
}

/// An answer to an agent's question: `yes`, `always` (yes, and don't ask again), `no`, or a question's option by its
/// place in the list, from 1. On the wire a word or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ChoiceRepr", into = "ChoiceRepr")]
pub enum Choice {
    Yes,
    Always,
    No,
    Option(NonZeroU32),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ChoiceRepr {
    Index(u32),
    Word(String),
}

impl TryFrom<ChoiceRepr> for Choice {
    type Error = String;

    fn try_from(repr: ChoiceRepr) -> Result<Self, String> {
        match repr {
            ChoiceRepr::Index(i) => NonZeroU32::new(i)
                .map(Self::Option)
                .ok_or_else(|| "a choice's number counts from 1".to_owned()),
            ChoiceRepr::Word(word) => word.parse(),
        }
    }
}

impl From<Choice> for ChoiceRepr {
    fn from(choice: Choice) -> Self {
        match choice {
            Choice::Option(i) => Self::Index(i.get()),
            word => Self::Word(word.to_string()),
        }
    }
}

impl std::str::FromStr for Choice {
    type Err = String;

    /// `yes`, `always`, `no`, or an option's number from 1.
    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "yes" => Ok(Self::Yes),
            "always" => Ok(Self::Always),
            "no" => Ok(Self::No),
            _ => text.parse::<NonZeroU32>().map(Self::Option).map_err(|_| {
                format!(
                    "{text:?} is not a choice: give yes, always, no, or an option's number from 1"
                )
            }),
        }
    }
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Yes => f.write_str("yes"),
            Self::Always => f.write_str("always"),
            Self::No => f.write_str("no"),
            Self::Option(i) => write!(f, "{i}"),
        }
    }
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

// Subagents: helpers the session's agent spawned (Claude Code's `Agent` tool, Codex's `spawn_agent`), as its hooks
// report them. `id` is the agent's own subagent id (`agent_id` in its hooks). Their text is the agent's, kept raw:
// display it only through `display_safe`.

/// A subagent started. Another for the same `id` corrects it: its type and task replace the earlier ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentStarted {
    pub id: String,
    /// What kind of subagent it is, as the agent names it (e.g. `Explore`, or a custom agent's name).
    pub agent_type: String,
    /// What it was asked to do: the spawning tool call's short description, or its prompt. Absent when no spawning
    /// call could be paired with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// A subagent is waiting on the user: it asked permission for a tool, or asked a question. It is answered in the
/// parent session's own TUI. A later `subagent/working`, `subagent/finished`, or the parent's turn ending ends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentBlocked {
    pub id: String,
    pub kind: BlockKind,
    /// What it is waiting on, for the user: the tool and its main argument, or the question.
    pub text: String,
    /// The subagent's type, as the hook said: it lets a block from a subagent the session no longer lists (e.g. a
    /// background one still running after the parent's turn ended) list it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// What a blocked subagent waits on. Open: a value this build doesn't know is kept as [`BlockKind::Other`], and is
/// still a block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockKind {
    /// Permission to use a tool.
    Permission,
    /// An answer to a question.
    Question,
    #[serde(untagged)]
    Other(String),
}

/// A blocked subagent got going again: its tool activity followed the block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentWorking {
    pub id: String,
}

/// A subagent finished. It stays listed until the parent's turn ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentFinished {
    pub id: String,
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
    /// The commit the branch started at, which the session's commits are counted from (absent from logs older than
    /// the archive, T42).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
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

/// The screen layout is now `layout` (host log; decision 0022): every window draws it. The latest one is current, with
/// the panes of sessions closed since showing nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLayout {
    pub layout: Layout,
}
