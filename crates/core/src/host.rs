//! The host's session registry: which sessions exist, their names, whether their PTY has exited, and whether an
//! exited one can be resumed.
//!
//! Resume: a session can be resumed when its PTY has exited, it runs an agent that can resume (Claude Code or Codex),
//! its `session/created` recorded the profile and cwd it started with, and its agent reported its own session id in an
//! `agent/started` (the latest one wins: `/clear` starts a new agent session in the same process). Resuming appends
//! the host's own `agent/started` and spawns the agent again in the same session, so its log continues.
//!
//! Workspace: the repo new sessions belong to, one per host. Selecting one appends `workspace/selected` to the host
//! log, so the last selection is the workspace on the next start, and every repo selected before is listed by
//! [`Host::workspaces`]. Whether a path is a repo is asked of the world: a select is first a
//! [`HostCommand::ProbeWorkspace`], whose answer comes back as [`HostEvent::WorkspaceProbed`]. A client's own cwd (from
//! its hello) is probed only while no workspace is selected, and selected only if there still is none. Workspaces are
//! repo roots as the probe resolved them, compared as paths, so selecting a subdirectory or a worktree of the current
//! workspace is the same workspace. A typed path may start with `~` ([`expand_home`]). Each `session/created` records
//! the workspace it was created in.
//!
//! Worktrees: a new session in a workspace, unless the request named its own cwd, gets a worktree of the workspace's
//! repo, on a branch named after the session ([`naming`]) and started from the origin default. Its `session/created`
//! records the branch and no cwd, and it is listed at once, waiting ([`ResumeUnavailable::Starting`], no PTY), while
//! [`HostCommand::CreateWorktree`] runs off the runtime thread. The result comes back as [`HostEvent::WorktreeCreated`]:
//! the worktree is recorded in `worktree/created`, whose path is the session's cwd from then on (resume uses it), and
//! the PTY spawns there. A failure is recorded as `session/failed`, and the session stays listed with the reason and no
//! PTY. Closing a session whose worktree is still being made cancels the create ([`HostCommand::CancelWorktree`]),
//! which kills its git and removes what it made. A result for a session closed meanwhile (the cancelled create's, or
//! one that finished first) is recorded the same way but spawns nothing; a created worktree is left in place for
//! cleanup to find through its record.
//!
//! Orphans (the finder, T26): a client can ask for the workspace's managed worktrees that no session that isn't closed
//! runs in or is having made ([`HostEvent::ListWorktrees`], answered from `Worktrees::list_managed` off the runtime
//! thread). A new session asked to run in one of them, with its branch, records both in its `session/created`: the
//! worktree is the session's own from the start, so closing it treats it like one the host made. A worktree that
//! another session that isn't closed uses (as its worktree or cwd) can't be started in this way, and closing a
//! session keeps a worktree another session still uses, saying so in its `session/closed`.
//!
//! Hooks: the workspace's post-create hooks ([`PostCreate`], from the config) run in a new worktree before its PTY
//! spawns. `worktree/created` records them, and the session stays starting while they run, one at a time, each a
//! [`HostCommand::RunHook`] off the runtime thread whose result comes back as [`HostEvent::HookFinished`] and is
//! recorded in `worktree/hook`. The first that fails (non-zero exit, timeout, signal, couldn't run) stops the rest and
//! marks the session with a warning, and the PTY spawns either way once the hooks are done. A session closed meanwhile
//! records the result it was waiting for and runs and spawns nothing more.
//!
//! Auto-naming: a session keeps the name it was created with (the runtime always asks for the placeholder,
//! `<profile>-<n>`) until its first user prompt (a `session/prompt` that isn't a continuation) is in its log. Once the
//! runtime has appended a hook's frames it feeds [`HostEvent::HookTranslated`], and if the session has a first prompt
//! that gives a name ([`naming::auto_name`]) and has never had a `session/renamed`, the host appends
//! `session/renamed { auto: true }`. Only the first prompt counts, so one with no meaningful words leaves the session
//! unnamed for good. Any rename, the user's own included, stops auto-naming, and since all of it is decided from the
//! log, a restart never names a session again. Only the display name changes: the branch keeps the name it was made
//! with.
//!
//! Closing: a session's PTY is killed at once. One with a worktree (made, and its post-create hooks done) is then
//! closing until what [`CloseWorktree`] asked for is done: `worktree/removing` records that it started, and it stays
//! listed, removing its worktree, until its `session/closed` says whether the worktree was removed, or why it was kept.
//! First [`HostCommand::InspectWorktree`] asks the `Worktrees` port whether the recorded path is one of the repo's
//! managed worktrees and what removing it would lose ([`WorktreeStatus::losses`]); a path the port doesn't list (one
//! that escapes the managed root with `..` or a symlink) is never touched, and one that is gone closes normally. If the
//! worktree loses nothing, or the user said to remove it anyway, the workspace's pre-remove hooks run there one at a
//! time like post-create hooks ([`HostCommand::RunPreRemove`], each recorded in `worktree/hook`), and then
//! [`HostCommand::RemoveWorktree`] removes it. Anything that fails (an inspection or removal error, a failing hook)
//! keeps the worktree, and `session/closed` says why. A host that restarts while a session is closing closes it,
//! keeping its worktree ([`HostEvent::Restarted`]). A client can ask what closing would lose first
//! ([`HostEvent::WorktreeStatus`]).
//!
//! Host state is only a fold over `_shepherd/*` frames and sessions' first user prompts (`docs/acpp.md`, "State is a
//! fold"). [`Host::handle`] reads state and decides what to do. It never changes state: every change is a
//! [`HostCommand::Append`] that the runtime seals, appends, and hands back to [`Host::apply`].

pub mod launch;
pub mod naming;
pub mod quiet;
pub mod upgrade;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::acpp::acp::{AcpFrame, ContentBlock, PromptRequest};
use crate::acpp::{
    AgentExited, AgentStarted, Dir, Envelope, Frame, FrameDraft, SessionClosed, SessionCreated,
    SessionFailed, SessionRenamed, ShepherdFrame, Source, SourceKind, Timestamp, WorkspaceSelected,
    WorktreeCreated, WorktreeHook, WorktreeRemoving,
};
use crate::agents::Translators;
use crate::config::{AgentKind, DEFAULT_HOOK_TIMEOUT, WorkspaceHooks};
use crate::ids::SessionId;
use crate::ports::commands::{CommandError, CommandExit, CommandOutput, CommandSpec, OUTPUT_TAIL};
use crate::ports::pty::{PtySize, SpawnSpec};
use crate::ports::worktrees::{
    BaseSpec, BaseUsed, Created, RepoRef, Worktree, WorktreeError, WorktreeName, WorktreePath,
    WorktreeStatus,
};
use crate::wire::{
    CloseWorktree, HostResult, InputKind, SessionNewOk, WireError, WorktreeInfo, WorktreeListOk,
    WorktreeStatusOk,
};

/// Identifies the client request a [`HostCommand::Reply`] answers. Opaque to core: the runtime assigns one to each
/// request it turns into a [`HostEvent`], and maps it back to the connection and JSON-RPC id when it replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReplyTo(u64);

impl ReplyTo {
    pub fn new(n: u64) -> Self {
        Self(n)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// What to start a new session with. The runtime fills it in from the session's agent profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionSpec {
    /// The session's name. `None` gives it a placeholder, `<profile>-<n>`, where `n` counts every session ever
    /// created on this host.
    pub name: Option<String>,
    /// The name of the agent profile it runs.
    pub profile: String,
    /// What kind of agent that profile runs, recorded in `session/created`.
    pub agent: AgentKind,
    /// The client asked for `spawn.cwd`. Otherwise a session in a workspace runs in a new worktree instead.
    pub explicit_cwd: bool,
    /// With `explicit_cwd`: `spawn.cwd` is a managed worktree on this branch (one [`HostEvent::ListWorktrees`] listed),
    /// and the session runs in it as its own worktree. Ignored without `explicit_cwd`.
    pub worktree_branch: Option<String>,
    pub spawn: SpawnSpec,
    /// The current workspace's hooks (`Config::workspace_hooks`). A new worktree runs their `post_create`.
    pub hooks: WorkspaceHooks,
}

/// Post-create hooks to run in a new worktree, in order, before its PTY spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostCreate {
    pub hooks: Vec<String>,
    /// How long each may run.
    pub timeout: Duration,
}

impl Default for PostCreate {
    fn default() -> Self {
        Self {
            hooks: Vec::new(),
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }
}

/// Pre-remove hooks to run in a worktree, in order, before it is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreRemove {
    pub hooks: Vec<String>,
    /// How long each may run.
    pub timeout: Duration,
}

/// Why a worktree is inspected ([`HostCommand::InspectWorktree`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspect {
    /// A client asked what removing it would lose: answer it.
    Ask(ReplyTo),
    /// Its session is closing: remove it after `hooks` if that loses nothing, or, with `confirmed` (what the user was
    /// shown and chose to remove anyway), if it loses nothing beyond that.
    Close {
        confirmed: Option<WorktreeStatus>,
        hooks: PreRemove,
    },
}

/// What [`HostCommand::InspectWorktree`] found at a session's recorded worktree path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    /// The port doesn't list it among the repo's managed worktrees. `exists`: something is at the path anyway (it isn't
    /// ours, so it is left alone); otherwise the worktree is gone.
    Unlisted { exists: bool },
    /// It is a managed worktree, with this status (or the error getting it). `exists`: something is at the path, so a
    /// [`WorktreeError::NotARepo`] doesn't mean it's gone.
    Listed {
        status: Result<WorktreeStatus, WorktreeError>,
        exists: bool,
    },
}

/// What to resume an exited session with. The runtime builds it from the profile in [`Host::resume_target`]
/// (`launch::plan_resume`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeSpec {
    /// The name of the agent profile it runs, which must be the one the session started with.
    pub profile: String,
    /// What kind of agent that profile runs now, which must be what the session started with.
    pub agent: AgentKind,
    pub spawn: SpawnSpec,
}

/// What resuming a session runs: [`Host::resume_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeTarget {
    /// The agent profile the session started with, by name.
    pub profile: String,
    pub agent: AgentKind,
    /// Where the session's agent started.
    pub cwd: PathBuf,
    /// The agent's own id for its session, to resume.
    pub agent_session_id: String,
}

/// Why a session can't be resumed. Its text is short, for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResumeUnavailable {
    /// Its PTY is still running.
    #[error("still running")]
    Running,
    /// It is waiting for its worktree, and its PTY hasn't started yet.
    #[error("still starting")]
    Starting,
    /// It failed to start (`session/failed`): it never ran an agent.
    #[error("it failed to start")]
    Failed,
    /// It is closing, removing its worktree.
    #[error("closing")]
    Closing,
    /// It doesn't run an agent that can resume, e.g. a shell.
    #[error("not an agent session")]
    NotAnAgent,
    /// Its `session/created` predates resume: it doesn't say which profile and directory to resume with.
    #[error("started before resume")]
    NotRecorded,
    /// Its agent never reported its own session id.
    #[error("no agent session id")]
    NoAgentSession,
}

/// Who asked for a workspace to be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAsk {
    /// A client's `_shepherd/workspace/select`: always selects a repo, and is answered.
    Select(ReplyTo),
    /// A client's cwd, from its hello: selected only while no workspace is, and never answered.
    ClientCwd,
}

/// Something that happened, for the host to decide on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A client asked for a new session. `id` and `at` come from the runtime, since core generates neither. The
    /// session's creation time in state is its `session/created` envelope's `ts`, stamped at append.
    NewSession {
        reply: ReplyTo,
        id: SessionId,
        at: Timestamp,
        spec: NewSessionSpec,
    },
    Rename {
        reply: ReplyTo,
        session: SessionId,
        name: String,
    },
    /// A client asked to close a session, doing `worktree` with its worktree. `hooks` are its workspace's hooks
    /// (`Config::workspace_hooks`), whose `pre_remove` run before a worktree is removed.
    Close {
        reply: ReplyTo,
        session: SessionId,
        worktree: CloseWorktree,
        hooks: WorkspaceHooks,
    },
    /// A client asked what removing a session's worktree would lose.
    WorktreeStatus { reply: ReplyTo, session: SessionId },
    /// A client asked for the current workspace's managed worktrees that no open session runs in (the finder).
    ListWorktrees { reply: ReplyTo },
    /// The answer to a [`HostCommand::ListWorktrees`].
    WorktreesListed {
        reply: ReplyTo,
        result: Result<Vec<Worktree>, WorktreeError>,
    },
    /// Bytes a client typed into a session, and what made them. A notification: never answered.
    Input {
        session: SessionId,
        bytes: Vec<u8>,
        kind: InputKind,
    },
    /// A client resized a session's terminal. A notification: never answered.
    Resize { session: SessionId, size: PtySize },
    /// `shepherd hook` sent a hook payload from an agent running in `session`.
    Hook {
        reply: ReplyTo,
        session: SessionId,
        agent: AgentKind,
        payload: Value,
    },
    /// The runtime appended (and applied) the frames a [`HostCommand::TranslateHook`] for `session` gave. A
    /// notification from the runtime: never answered.
    HookTranslated { session: SessionId },
    /// A session's PTY process ended. `code` is `None` when a signal ended it.
    PtyExited {
        session: SessionId,
        code: Option<i32>,
    },
    /// A client asked to resume an exited session's agent.
    Resume {
        reply: ReplyTo,
        session: SessionId,
        spec: ResumeSpec,
    },
    /// `path` should be the workspace, see [`WorkspaceAsk`].
    SelectWorkspace { ask: WorkspaceAsk, path: PathBuf },
    /// The answer to a [`HostCommand::ProbeWorkspace`]: the repo the path resolved to, or why it isn't one, for the
    /// user.
    WorkspaceProbed {
        ask: WorkspaceAsk,
        repo: Result<PathBuf, String>,
    },
    /// The answer to a [`HostCommand::CreateWorktree`], with the `spawn` and `hooks` it carried.
    WorktreeCreated {
        session: SessionId,
        spawn: SpawnSpec,
        hooks: PostCreate,
        result: Result<Created, WorktreeError>,
    },
    /// The answer to a [`HostCommand::RunHook`], with everything it carried.
    HookFinished {
        session: SessionId,
        command: CommandSpec,
        rest: PostCreate,
        spawn: SpawnSpec,
        result: Result<CommandOutput, CommandError>,
    },
    /// The answer to a [`HostCommand::InspectWorktree`], with everything it carried. The outer error is the port
    /// failing to list the repo's worktrees.
    WorktreeInspected {
        session: SessionId,
        path: WorktreePath,
        purpose: Inspect,
        result: Result<Inspection, WorktreeError>,
    },
    /// The answer to a [`HostCommand::RunPreRemove`], with everything it carried.
    PreRemoveFinished {
        session: SessionId,
        command: CommandSpec,
        rest: PreRemove,
        path: WorktreePath,
        force: bool,
        result: Result<CommandOutput, CommandError>,
    },
    /// The answer to a [`HostCommand::RemoveWorktree`]. `exists`: something is still at the path afterwards.
    WorktreeRemoved {
        session: SessionId,
        path: WorktreePath,
        result: Result<(), WorktreeError>,
        exists: bool,
    },
    /// The runtime has just started, the logs folded: nothing an earlier host run started is still running.
    Restarted,
}

/// Something for the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum HostCommand {
    /// Seal and append this frame to its log, then pass the envelope to [`Host::apply`].
    Append(Box<FrameDraft>),
    SpawnPty {
        session: SessionId,
        spec: SpawnSpec,
    },
    /// Write a client's input to a session's PTY, then tell the [`QuietWatch`](quiet::QuietWatch) about it.
    WritePty {
        session: SessionId,
        bytes: Vec<u8>,
        kind: InputKind,
    },
    ResizePty {
        session: SessionId,
        size: PtySize,
    },
    KillPty {
        session: SessionId,
    },
    /// Translate a hook payload with the session's translator in [`Translators`], then seal, append and apply
    /// each frame it gives, as for [`HostCommand::Append`].
    TranslateHook {
        session: SessionId,
        agent: AgentKind,
        payload: Value,
    },
    /// Answer the client request `to`.
    Reply {
        to: ReplyTo,
        result: Result<HostResult, WireError>,
    },
    /// Find out which git repo `path` is in, as the root of its main checkout (resolving `..` and symlinks), and feed
    /// the answer back as [`HostEvent::WorkspaceProbed`] with the same `ask`. The probe runs off the runtime thread, so
    /// other events may be handled before its answer arrives, and the answer may no longer matter by then.
    ProbeWorkspace {
        ask: WorkspaceAsk,
        path: PathBuf,
    },
    /// Create worktree `name` of `repo` from `base`, and feed the result back as [`HostEvent::WorktreeCreated`] with
    /// the same `session`, `spawn` and `hooks`: what to spawn once the worktree exists (its cwd is replaced by the
    /// worktree's path), and the hooks to run there first. Runs off the runtime thread, like
    /// [`HostCommand::ProbeWorkspace`].
    CreateWorktree {
        session: SessionId,
        repo: RepoRef,
        name: WorktreeName,
        base: BaseSpec,
        spawn: SpawnSpec,
        hooks: PostCreate,
    },
    /// Stop the [`HostCommand::CreateWorktree`] still running for `session` (`Worktrees::cancel_create`): its git is
    /// killed and what it made removed, and its answer comes back as usual, as [`WorktreeError::Cancelled`]. Never
    /// waits on git.
    CancelWorktree {
        session: SessionId,
    },
    /// Run a post-create hook with the `Commands` port, and feed the result back as [`HostEvent::HookFinished`] with
    /// the same `session`, `command`, `rest` (the hooks after it) and `spawn` (what to spawn once they are done). Runs
    /// off the runtime thread, like [`HostCommand::ProbeWorkspace`].
    RunHook {
        session: SessionId,
        command: CommandSpec,
        rest: PostCreate,
        spawn: SpawnSpec,
    },
    /// Find out whether `path` is one of `repo`'s managed worktrees (`Worktrees::list_managed`), and if so its status,
    /// and feed the answer back as [`HostEvent::WorktreeInspected`] with the same `session`, `path` and `purpose`.
    /// Runs off the runtime thread, like [`HostCommand::ProbeWorkspace`].
    InspectWorktree {
        session: SessionId,
        repo: RepoRef,
        path: WorktreePath,
        purpose: Inspect,
    },
    /// List `repo`'s managed worktrees (`Worktrees::list_managed`), and feed the answer back as
    /// [`HostEvent::WorktreesListed`] with the same `reply`. Runs off the runtime thread, like
    /// [`HostCommand::ProbeWorkspace`].
    ListWorktrees {
        reply: ReplyTo,
        repo: RepoRef,
    },
    /// Run a pre-remove hook with the `Commands` port, and feed the result back as [`HostEvent::PreRemoveFinished`]
    /// with the same `session`, `command`, `rest` (the hooks after it), `path` and `force` (how to remove the worktree
    /// once they are done). Runs off the runtime thread, like [`HostCommand::RunHook`].
    RunPreRemove {
        session: SessionId,
        command: CommandSpec,
        rest: PreRemove,
        path: WorktreePath,
        force: bool,
    },
    /// Remove worktree `path` (`Worktrees::remove`), and feed the result back as [`HostEvent::WorktreeRemoved`]. Runs
    /// off the runtime thread, like [`HostCommand::ProbeWorkspace`].
    RemoveWorktree {
        session: SessionId,
        path: WorktreePath,
        force: bool,
    },
}

/// A session as [`Host::sessions`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSession {
    pub id: SessionId,
    pub name: String,
    /// The `ts` of its `session/created` entry.
    pub created: Timestamp,
    /// Its PTY process has ended, or it failed before one started.
    pub exited: bool,
    /// Whether it can be resumed, see [`Host::resume_target`].
    pub resume: Result<(), ResumeUnavailable>,
    /// The workspace it was created in. `None` if there was none, or it predates workspaces.
    pub workspace: Option<PathBuf>,
    /// Why it failed to start (`session/failed`), for the user.
    pub failed: Option<String>,
    /// It is waiting for its worktree to be made or its post-create hooks to run, and has no PTY yet.
    pub creating_worktree: bool,
    /// Something went wrong that didn't stop it starting, e.g. a post-create hook failed, for the user.
    pub warning: Option<String>,
    /// Its worktree, once made (`worktree/created`), or the one it was started in.
    pub worktree: Option<PathBuf>,
    /// Its worktree's branch: the one it was given while its worktree is being made, then its worktree's.
    pub branch: Option<String>,
    /// It is closing, and its worktree is being dealt with. Its PTY is gone.
    pub removing_worktree: bool,
}

/// Where a session is in its life, as far as its PTY goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Waiting for its worktree. No PTY yet.
    Starting,
    /// Its worktree exists, and this many of its post-create hooks have yet to finish. No PTY yet.
    Hooks {
        left: usize,
    },
    Running,
    Exited,
    /// It never got a PTY, for this reason.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    /// With the session id, orders [`Host::sessions`].
    created: Timestamp,
    phase: Phase,
    closed: bool,
    /// From `session/created`: what it runs, and how it was started (absent from logs older than resume).
    agent: Option<AgentKind>,
    profile: Option<String>,
    cwd: Option<PathBuf>,
    /// The agent's own session id, from the latest `agent/started` that had one.
    agent_session_id: Option<String>,
    workspace: Option<PathBuf>,
    warning: Option<String>,
    /// It has a `session/renamed`, by the user or by shepherd: it is never auto-named again.
    renamed: bool,
    /// Its first user prompt (not a continuation) has been folded.
    prompted: bool,
    /// The name its first user prompt gives it ([`naming::auto_name`]), if any.
    auto_name: Option<String>,
    /// Its worktree's path and branch, from `worktree/created`, or the `session/created` of a session started in one.
    worktree: Option<(PathBuf, String)>,
    /// The branch its `session/created` gave it, until its worktree says.
    branch: Option<String>,
    /// Closing it is removing its worktree (`worktree/removing`), until its `session/closed`.
    removing: bool,
}

impl Entry {
    /// Its worktree, if it has one closing could remove: made, with its post-create hooks done.
    fn removable_worktree(&self) -> Option<&(PathBuf, String)> {
        match self.phase {
            Phase::Starting | Phase::Hooks { .. } => None,
            _ => self.worktree.as_ref(),
        }
    }

    fn resume_target(&self) -> Result<ResumeTarget, ResumeUnavailable> {
        if self.removing {
            return Err(ResumeUnavailable::Closing);
        }
        match self.phase {
            Phase::Starting | Phase::Hooks { .. } => return Err(ResumeUnavailable::Starting),
            Phase::Running => return Err(ResumeUnavailable::Running),
            Phase::Failed(_) => return Err(ResumeUnavailable::Failed),
            Phase::Exited => {}
        }
        let agent = self
            .agent
            .filter(|a| matches!(a, AgentKind::ClaudeCode | AgentKind::Codex))
            .ok_or(ResumeUnavailable::NotAnAgent)?;
        let (Some(profile), Some(cwd)) = (&self.profile, &self.cwd) else {
            return Err(ResumeUnavailable::NotRecorded);
        };
        let agent_session_id = self
            .agent_session_id
            .clone()
            .ok_or(ResumeUnavailable::NoAgentSession)?;
        Ok(ResumeTarget {
            profile: profile.clone(),
            agent,
            cwd: cwd.clone(),
            agent_session_id,
        })
    }
}

/// The host's state and decisions.
///
/// Contract with the runtime: for each [`HostEvent`], call [`Host::handle`] and execute the commands in order. Each
/// [`HostCommand::Append`] goes through the event log, and the sealed envelope it returns is passed to
/// [`Host::apply`] before the next event is handled, so a `NewSession` followed by a `Rename` of it works.
///
/// On start, the runtime replays the logs through `apply`. Logs may be replayed in any interleaving; entries within
/// one log must be applied in seq order. Every such interleaving folds to the same state: nothing in it depends on
/// the order in which entries from different logs arrive.
///
/// When something fails in the runtime:
/// - If [`HostCommand::SpawnPty`] fails, the runtime feeds `HostEvent::PtyExited { session, code: None }`, so the
///   session shows as exited and closing it won't send [`HostCommand::KillPty`].
/// - On start, it feeds the same for every listed session that isn't exited: a PTY never survives a restart, and a
///   session still waiting for its worktree is failed, since the old host's git call answers nobody.
/// - If [`HostCommand::CreateWorktree`] can't run (its worker is gone or the call panics), the runtime feeds a
///   [`HostEvent::WorktreeCreated`] with an error, so the session fails instead of waiting forever.
/// - If an [`HostCommand::Append`] fails, the runtime skips the rest of that `handle` call's commands. If they
///   include a [`HostCommand::Reply`], it sends that reply as `Err(WireError::Internal)` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Host {
    /// Every session ever created, closed ones included, so a closed id stays unknown to requests and still counts
    /// towards placeholder names.
    sessions: BTreeMap<SessionId, Entry>,
    /// Every workspace ever selected, the current one (the latest selected) first.
    workspaces: Vec<PathBuf>,
    /// Every branch a session was given (`session/created`) or got (`worktree/created`), closed sessions' included.
    branches: BTreeSet<String>,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open sessions, newest first: by the instant of their `session/created` entry's `ts`, then by session id.
    pub fn sessions(&self) -> Vec<HostSession> {
        let mut open: Vec<_> = self.sessions.iter().filter(|(_, e)| !e.closed).collect();
        open.sort_by(|(a_id, a), (b_id, b)| (&b.created, *b_id).cmp(&(&a.created, *a_id)));
        open.into_iter()
            .map(|(id, e)| HostSession {
                id: id.clone(),
                name: e.name.clone(),
                created: e.created.clone(),
                exited: e.removing || matches!(e.phase, Phase::Exited | Phase::Failed(_)),
                resume: e.resume_target().map(|_| ()),
                workspace: e.workspace.clone(),
                failed: match &e.phase {
                    Phase::Failed(reason) => Some(reason.clone()),
                    _ => None,
                },
                creating_worktree: matches!(e.phase, Phase::Starting | Phase::Hooks { .. }),
                warning: e.warning.clone(),
                worktree: e.worktree.as_ref().map(|(path, _)| path.clone()),
                branch: match &e.worktree {
                    Some((_, branch)) => Some(branch.clone()),
                    None => e.branch.clone(),
                },
                removing_worktree: e.removing,
            })
            .collect()
    }

    /// The selected workspace, if any.
    pub fn workspace(&self) -> Option<&Path> {
        self.workspaces.first().map(PathBuf::as_path)
    }

    /// Every workspace selected on this host, each once, the current one first and then by when it was last selected.
    pub fn workspaces(&self) -> &[PathBuf] {
        &self.workspaces
    }

    /// What resuming open session `id` runs, or why it can't be resumed. `None` for a session that isn't open.
    pub fn resume_target(&self, id: &SessionId) -> Option<Result<ResumeTarget, ResumeUnavailable>> {
        self.open(id).map(Entry::resume_target)
    }

    /// The workspace open session `id` was created in, if any. `None` for a session that isn't open.
    pub fn session_workspace(&self, id: &SessionId) -> Option<&Path> {
        self.open(id).and_then(|e| e.workspace.as_deref())
    }

    /// Decides what to do about `event`. Never changes state, see the type docs.
    pub fn handle(&self, event: HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::NewSession {
                reply,
                id,
                at: _,
                spec,
            } => {
                if self.sessions.contains_key(&id) {
                    let error = WireError::Internal(format!("session {id} already exists"));
                    return vec![reply_err(reply, error)];
                }
                // The placeholder is picked here and written into the frame. The fold only reads it back.
                let name = spec
                    .name
                    .unwrap_or_else(|| format!("{}-{}", spec.profile, self.sessions.len() + 1));
                let workspace = self.workspace().and_then(Path::to_str);
                // A worktree is made in the workspace unless the client chose where to run.
                let hooks = PostCreate {
                    hooks: spec.hooks.post_create,
                    timeout: spec.hooks.timeout,
                };
                let worktree = workspace.filter(|_| !spec.explicit_cwd).map(|repo| {
                    (
                        RepoRef::new(repo),
                        naming::worktree_name(&name, &self.branches),
                    )
                });
                // A worktree the client chose to run in (the finder's orphans) is the session's own from the start.
                let adopted = match (
                    spec.explicit_cwd,
                    spec.worktree_branch,
                    spec.spawn.cwd.to_str(),
                ) {
                    (true, Some(branch), Some(path)) => Some((path.to_owned(), branch)),
                    _ => None,
                };
                // Two sessions owning one worktree would let closing one remove it under the other.
                if let Some(user) = adopted
                    .as_ref()
                    .and_then(|(path, _)| self.used_by(None, Path::new(path)))
                {
                    let error = WireError::InvalidParams(format!(
                        "{} is already in use by {user}",
                        spec.spawn.cwd.display()
                    ));
                    return vec![reply_err(reply, error)];
                }
                let branch = match (&worktree, &adopted) {
                    (Some((_, name)), _) => Some(name.as_str().to_owned()),
                    (None, Some((_, branch))) => Some(branch.clone()),
                    (None, None) => None,
                };
                let created = ShepherdFrame::SessionCreated(SessionCreated {
                    name,
                    workspace: workspace.map(str::to_owned),
                    worktree: adopted.map(|(path, _)| path),
                    branch,
                    agent: Some(spec.agent.as_str().to_owned()),
                    machine: None,
                    profile: Some(spec.profile),
                    // A worktree session's cwd is its worktree, recorded once it exists. A cwd that isn't UTF-8 isn't
                    // recorded, so the session can't be resumed.
                    cwd: match worktree {
                        Some(_) => None,
                        None => spec.spawn.cwd.to_str().map(str::to_owned),
                    },
                });
                let start = match worktree {
                    Some((repo, name)) => HostCommand::CreateWorktree {
                        session: id.clone(),
                        repo,
                        name,
                        base: BaseSpec::OriginDefault,
                        spawn: spec.spawn,
                        hooks,
                    },
                    None => HostCommand::SpawnPty {
                        session: id.clone(),
                        spec: spec.spawn,
                    },
                };
                vec![
                    append(&id, created),
                    start,
                    HostCommand::Reply {
                        to: reply,
                        result: Ok(HostResult::SessionNew(SessionNewOk { session: id })),
                    },
                ]
            }
            HostEvent::Rename {
                reply,
                session,
                name,
            } => {
                if self.open(&session).is_none() {
                    return vec![unknown_session(reply, session)];
                }
                let renamed = ShepherdFrame::SessionRenamed(SessionRenamed { name, auto: false });
                vec![append(&session, renamed), ack(reply)]
            }
            HostEvent::Close {
                reply,
                session,
                worktree,
                hooks,
            } => self.close(reply, session, worktree, hooks),
            HostEvent::WorktreeStatus { reply, session } => {
                let Some(entry) = self.open(&session) else {
                    return vec![unknown_session(reply, session)];
                };
                let (Some((path, _)), Some(repo)) =
                    (entry.removable_worktree(), entry.workspace.as_ref())
                else {
                    let error =
                        WireError::InvalidParams(format!("{session} has no worktree to remove"));
                    return vec![reply_err(reply, error)];
                };
                vec![HostCommand::InspectWorktree {
                    session,
                    repo: RepoRef::new(repo),
                    path: WorktreePath::new(path),
                    purpose: Inspect::Ask(reply),
                }]
            }
            HostEvent::ListWorktrees { reply } => match self.workspace() {
                Some(repo) => vec![HostCommand::ListWorktrees {
                    reply,
                    repo: RepoRef::new(repo),
                }],
                None => vec![HostCommand::Reply {
                    to: reply,
                    result: Ok(HostResult::WorktreeList(WorktreeListOk::default())),
                }],
            },
            HostEvent::WorktreesListed { reply, result } => vec![HostCommand::Reply {
                to: reply,
                result: match result {
                    Ok(worktrees) => Ok(HostResult::WorktreeList(self.orphans(worktrees))),
                    Err(error) => Err(WireError::InvalidParams(error.to_string())),
                },
            }],
            HostEvent::Hook {
                reply,
                session,
                agent,
                payload,
            } => {
                if self.open(&session).is_none() {
                    return vec![unknown_session(reply, session)];
                }
                if !Translators::supports(agent) {
                    let error = WireError::InvalidParams(format!(
                        "no hook translator for {}",
                        agent.as_str()
                    ));
                    return vec![reply_err(reply, error)];
                }
                vec![
                    HostCommand::TranslateHook {
                        session,
                        agent,
                        payload,
                    },
                    ack(reply),
                ]
            }
            // The runtime's restart sweep: a session still waiting for its worktree lost it with the old host.
            HostEvent::PtyExited { session, .. } if self.starting(&session) => {
                let reason = match self.open(&session).map(|e| &e.phase) {
                    Some(Phase::Hooks { .. }) => {
                        "the host stopped before the worktree hooks finished"
                    }
                    _ => "the host stopped before the worktree was ready",
                };
                let failed = ShepherdFrame::SessionFailed(SessionFailed {
                    reason: reason.to_owned(),
                });
                vec![append(&session, failed)]
            }
            // Input, resize and exits for a session that isn't running are dropped: notifications are never
            // answered, and a PTY exits once.
            HostEvent::Input { session, .. }
            | HostEvent::Resize { session, .. }
            | HostEvent::PtyExited { session, .. }
                if !self.running(&session) =>
            {
                Vec::new()
            }
            HostEvent::Input {
                session,
                bytes,
                kind,
            } => vec![HostCommand::WritePty {
                session,
                bytes,
                kind,
            }],
            HostEvent::Resize { session, size } => vec![HostCommand::ResizePty { session, size }],
            HostEvent::PtyExited { session, code } => {
                let exited = ShepherdFrame::AgentExited(AgentExited { code, signal: None });
                vec![append(&session, exited)]
            }
            HostEvent::Resume {
                reply,
                session,
                spec,
            } => {
                let target = match self.resume_target(&session) {
                    None => return vec![unknown_session(reply, session)],
                    Some(Err(why)) => {
                        let error =
                            WireError::InvalidParams(format!("can't resume {session}: {why}"));
                        return vec![reply_err(reply, error)];
                    }
                    Some(Ok(target)) => target,
                };
                if spec.profile != target.profile || spec.agent != target.agent {
                    let error = WireError::InvalidParams(format!(
                        "can't resume {session}: it started as profile `{}` ({}), not `{}` ({})",
                        target.profile,
                        target.agent.as_str(),
                        spec.profile,
                        spec.agent.as_str()
                    ));
                    return vec![reply_err(reply, error)];
                }
                let started = ShepherdFrame::AgentStarted(AgentStarted {
                    agent: target.agent.as_str().to_owned(),
                    agent_session_id: Some(target.agent_session_id),
                    pid: None,
                    command: None,
                });
                vec![
                    append(&session, started),
                    HostCommand::SpawnPty {
                        session,
                        spec: spec.spawn,
                    },
                    ack(reply),
                ]
            }
            HostEvent::SelectWorkspace {
                ask: WorkspaceAsk::ClientCwd,
                ..
            } if self.workspace().is_some() => Vec::new(),
            HostEvent::SelectWorkspace { ask, path } => {
                vec![HostCommand::ProbeWorkspace { ask, path }]
            }
            HostEvent::WorkspaceProbed { ask, repo } => self.workspace_probed(ask, repo),
            HostEvent::WorktreeCreated {
                session,
                spawn,
                hooks,
                result,
            } => self.worktree_created(session, spawn, hooks, result),
            HostEvent::HookFinished {
                session,
                command,
                rest,
                spawn,
                result,
            } => self.hook_finished(session, command, rest, spawn, result),
            HostEvent::HookTranslated { session } => self.auto_rename(session),
            HostEvent::WorktreeInspected {
                session: _,
                path,
                purpose: Inspect::Ask(reply),
                result,
            } => vec![HostCommand::Reply {
                to: reply,
                result: status_answer(&path, result),
            }],
            HostEvent::WorktreeInspected {
                session,
                path,
                purpose: Inspect::Close { confirmed, hooks },
                result,
            } => self.worktree_inspected(session, path, confirmed, hooks, result),
            HostEvent::PreRemoveFinished {
                session,
                command,
                rest,
                path,
                force,
                result,
            } => self.pre_remove_finished(session, command, rest, path, force, result),
            HostEvent::WorktreeRemoved {
                session,
                path: _,
                result,
                exists,
            } => {
                if !self.removing(&session) {
                    return Vec::new();
                }
                match result {
                    Ok(()) => vec![closed(&session, true, None)],
                    // Already gone: nothing was lost and nothing is left.
                    Err(error) if is_gone(&error, exists) => vec![closed(&session, false, None)],
                    Err(error) => vec![closed(&session, false, Some(error.to_string()))],
                }
            }
            HostEvent::Restarted => self
                .sessions
                .iter()
                .filter(|(_, e)| e.removing && !e.closed)
                .map(|(id, _)| {
                    let why = "the host stopped before the worktree was removed";
                    closed(id, false, Some(why.to_owned()))
                })
                .collect(),
        }
    }

    /// Closes open session `session`: kills its PTY (or cancels the create of a worktree still being made, whose
    /// cleanup is the port's), then closes it at once, or, if it has a worktree to remove, records that and inspects
    /// the worktree first.
    fn close(
        &self,
        reply: ReplyTo,
        session: SessionId,
        worktree: CloseWorktree,
        hooks: WorkspaceHooks,
    ) -> Vec<HostCommand> {
        let Some(entry) = self.open(&session) else {
            return vec![unknown_session(reply, session)];
        };
        let mut commands = Vec::new();
        match entry.phase {
            Phase::Running => commands.push(HostCommand::KillPty {
                session: session.clone(),
            }),
            Phase::Starting => commands.push(HostCommand::CancelWorktree {
                session: session.clone(),
            }),
            _ => {}
        }
        let confirmed = match worktree {
            CloseWorktree::Remove { confirmed } => Some(confirmed),
            CloseWorktree::IfSafe | CloseWorktree::Keep => None,
        };
        let removable = entry.removable_worktree().zip(entry.workspace.as_ref());
        let (Some(((path, _), repo)), false) = (removable, worktree == CloseWorktree::Keep) else {
            commands.push(closed(&session, false, None));
            commands.push(ack(reply));
            return commands;
        };
        if let Some(why) = self.still_used(&session, path) {
            commands.push(closed(&session, false, Some(why)));
            commands.push(ack(reply));
            return commands;
        }
        let removing = ShepherdFrame::WorktreeRemoving(WorktreeRemoving {
            path: path.to_string_lossy().into_owned(),
            force: confirmed.is_some(),
            pre_remove: hooks.pre_remove.clone(),
        });
        commands.push(append(&session, removing));
        commands.push(HostCommand::InspectWorktree {
            session: session.clone(),
            repo: RepoRef::new(repo),
            path: WorktreePath::new(path),
            purpose: Inspect::Close {
                confirmed,
                hooks: PreRemove {
                    hooks: hooks.pre_remove,
                    timeout: hooks.timeout,
                },
            },
        });
        commands.push(ack(reply));
        commands
    }

    /// Decides on a closing session's worktree from what inspecting it found: remove it (its pre-remove hooks first),
    /// or close keeping it. Without `confirmed` it is removed only if that loses nothing; with it, only if that loses
    /// nothing the user wasn't shown, and the removal is forced only if the user confirmed losing uncommitted changes.
    fn worktree_inspected(
        &self,
        session: SessionId,
        path: WorktreePath,
        confirmed: Option<WorktreeStatus>,
        hooks: PreRemove,
        result: Result<Inspection, WorktreeError>,
    ) -> Vec<HostCommand> {
        if !self.removing(&session) {
            return Vec::new();
        }
        // Another session may have started in it since the close began.
        if let Some(why) = self.still_used(&session, path.as_path()) {
            return vec![closed(&session, false, Some(why))];
        }
        let status = match result {
            Ok(Inspection::Listed {
                status: Ok(status), ..
            }) => status,
            Ok(Inspection::Listed {
                status: Err(error),
                exists,
            }) if is_gone(&error, exists) => {
                return vec![closed(&session, false, None)];
            }
            Ok(Inspection::Unlisted { exists: false }) => {
                return vec![closed(&session, false, None)];
            }
            Ok(Inspection::Unlisted { exists: true }) => {
                let why = format!("{path} isn't a worktree shepherd manages");
                return vec![closed(&session, false, Some(why))];
            }
            Err(error)
            | Ok(Inspection::Listed {
                status: Err(error), ..
            }) => {
                return vec![closed(&session, false, Some(error.to_string()))];
            }
        };
        let (unconfirmed, why) = match confirmed {
            None => (status.losses(), ""),
            Some(confirmed) => (
                status.losses_beyond(&confirmed),
                ", which closing didn't confirm",
            ),
        };
        if !unconfirmed.is_empty() {
            let lost: Vec<String> = unconfirmed.iter().map(ToString::to_string).collect();
            let why = format!("it has {}{why}", lost.join(" and "));
            return vec![closed(&session, false, Some(why))];
        }
        let force = confirmed.is_some_and(|c| c.dirty);
        vec![self.next_pre_remove(session, path, force, hooks)]
    }

    /// Runs the first of `hooks` in closing session `session`'s worktree at `path`, or with none left removes it.
    fn next_pre_remove(
        &self,
        session: SessionId,
        path: WorktreePath,
        force: bool,
        mut hooks: PreRemove,
    ) -> HostCommand {
        if hooks.hooks.is_empty() {
            return HostCommand::RemoveWorktree {
                session,
                path,
                force,
            };
        }
        let script = hooks.hooks.remove(0);
        let entry = self.sessions.get(&session);
        let branch = entry
            .and_then(|e| e.worktree.as_ref())
            .map(|(_, branch)| branch.as_str())
            .unwrap_or_default();
        let workspace = entry.and_then(|e| e.workspace.as_deref());
        let command = CommandSpec {
            script,
            cwd: path.as_path().to_path_buf(),
            env: hook_env(&session, &path.to_string(), branch, workspace),
            timeout: hooks.timeout,
        };
        HostCommand::RunPreRemove {
            session,
            command,
            rest: hooks,
            path,
            force,
        }
    }

    /// Records a pre-remove hook's result, then runs the next one or removes the worktree, or, if it failed, closes the
    /// session keeping its worktree. Only a closing session takes a result.
    fn pre_remove_finished(
        &self,
        session: SessionId,
        command: CommandSpec,
        rest: PreRemove,
        path: WorktreePath,
        force: bool,
        result: Result<CommandOutput, CommandError>,
    ) -> Vec<HostCommand> {
        if !self.removing(&session) {
            return Vec::new();
        }
        let recorded = hook_record(&command, result);
        let failure = hook_failure(&recorded);
        let why = failure.map(|why| format!("pre_remove hook failed ({why}): {}", recorded.hook));
        let mut commands = vec![append(&session, ShepherdFrame::WorktreeHook(recorded))];
        match why {
            Some(why) => commands.push(closed(&session, false, Some(why))),
            None => commands.push(self.next_pre_remove(session, path, force, rest)),
        }
        commands
    }

    /// Names an open session after its first user prompt, once that prompt is in the log, unless it was ever renamed.
    fn auto_rename(&self, session: SessionId) -> Vec<HostCommand> {
        let Some(name) = self
            .open(&session)
            .filter(|e| !e.renamed)
            .and_then(|e| e.auto_name.clone())
        else {
            return Vec::new();
        };
        let renamed = ShepherdFrame::SessionRenamed(SessionRenamed { name, auto: true });
        vec![append(&session, renamed)]
    }

    /// Records what came of a session's worktree, and if the session is still open runs its first post-create hook
    /// there, or with none spawns its PTY there. Only a session waiting for its worktree takes a result.
    fn worktree_created(
        &self,
        session: SessionId,
        mut spawn: SpawnSpec,
        hooks: PostCreate,
        result: Result<Created, WorktreeError>,
    ) -> Vec<HostCommand> {
        let Some(entry) = self.sessions.get(&session) else {
            return Vec::new();
        };
        if entry.phase != Phase::Starting {
            return Vec::new();
        }
        let created = result.and_then(|created| match created.path.as_path().to_str() {
            Some(path) => Ok((path.to_owned(), created)),
            None => Err(WorktreeError::Other(format!(
                "the worktree path {} is not UTF-8",
                created.path
            ))),
        });
        let (path, created) = match created {
            Ok(created) => created,
            Err(error) => {
                let failed = ShepherdFrame::SessionFailed(SessionFailed {
                    reason: error.to_string(),
                });
                return vec![append(&session, failed)];
            }
        };
        let (base, fetch_error) = match created.base {
            BaseUsed::Origin { branch } => (format!("origin/{branch}"), None),
            BaseUsed::Stale {
                branch,
                fetch_error,
            } => (format!("origin/{branch}"), Some(fetch_error)),
            BaseUsed::Local {
                branch,
                fetch_error,
            } => (branch.to_string(), Some(fetch_error)),
        };
        let recorded = ShepherdFrame::WorktreeCreated(WorktreeCreated {
            path: path.clone(),
            branch: created.branch.to_string(),
            base,
            fetch_error,
            post_create: hooks.hooks.clone(),
        });
        let mut commands = vec![append(&session, recorded)];
        if entry.closed {
            return commands;
        }
        spawn.cwd = created.path.as_path().to_path_buf();
        let mut rest = hooks;
        if rest.hooks.is_empty() {
            commands.push(HostCommand::SpawnPty {
                session,
                spec: spawn,
            });
            return commands;
        }
        let env = hook_env(
            &session,
            &path,
            created.branch.as_str(),
            entry.workspace.as_deref(),
        );
        let script = rest.hooks.remove(0);
        commands.push(HostCommand::RunHook {
            session,
            command: CommandSpec {
                script,
                cwd: spawn.cwd.clone(),
                env,
                timeout: rest.timeout,
            },
            rest,
            spawn,
        });
        commands
    }

    /// Records a post-create hook's result, then if the session is still open runs the next hook, or once they are
    /// done (or this one failed) spawns its PTY. Only a session running its hooks takes a result.
    fn hook_finished(
        &self,
        session: SessionId,
        command: CommandSpec,
        mut rest: PostCreate,
        spawn: SpawnSpec,
        result: Result<CommandOutput, CommandError>,
    ) -> Vec<HostCommand> {
        let Some(entry) = self.sessions.get(&session) else {
            return Vec::new();
        };
        if !matches!(entry.phase, Phase::Hooks { .. }) {
            return Vec::new();
        }
        let recorded = hook_record(&command, result);
        let succeeded = hook_failure(&recorded).is_none();
        let mut commands = vec![append(&session, ShepherdFrame::WorktreeHook(recorded))];
        if entry.closed {
            return commands;
        }
        if succeeded && !rest.hooks.is_empty() {
            let script = rest.hooks.remove(0);
            commands.push(HostCommand::RunHook {
                session,
                command: CommandSpec { script, ..command },
                rest,
                spawn,
            });
        } else {
            commands.push(HostCommand::SpawnPty {
                session,
                spec: spawn,
            });
        }
        commands
    }

    fn workspace_probed(
        &self,
        ask: WorkspaceAsk,
        repo: Result<PathBuf, String>,
    ) -> Vec<HostCommand> {
        let reply = match ask {
            WorkspaceAsk::Select(reply) => Some(reply),
            WorkspaceAsk::ClientCwd if self.workspace().is_some() => return Vec::new(),
            WorkspaceAsk::ClientCwd => None,
        };
        let repo = repo.and_then(|repo| match repo.to_str() {
            Some(text) => Ok((text.to_owned(), repo)),
            None => Err(format!("{} is not a UTF-8 path", repo.display())),
        });
        let mut commands = Vec::new();
        match repo {
            Ok((_, repo)) if self.workspace() == Some(repo.as_path()) => {}
            Ok((text, _)) => {
                let selected = ShepherdFrame::WorkspaceSelected(WorkspaceSelected { repo: text });
                commands.push(append_host(selected));
            }
            Err(why) => {
                if let Some(reply) = reply {
                    commands.push(reply_err(reply, WireError::InvalidParams(why)));
                }
                return commands;
            }
        }
        commands.extend(reply.map(ack));
        commands
    }

    /// Folds one log entry into the state. Entries the registry doesn't read change nothing, and no entry panics. Of
    /// the ACP frames, only a session's first user prompt is read, for auto-naming.
    ///
    /// Only the host's own `agent/exited` (source `shepherd`) marks a PTY exited: one from a hook is about an agent
    /// the shell ran, and the shell is still running. Likewise only the host's own `agent/started` (a resume) marks
    /// it running again. An `agent/started` from any source records its agent session id.
    pub fn apply(&mut self, env: &Envelope) {
        let message = match env.frame() {
            Frame::Shepherd(message) => message,
            Frame::Acp(message) => {
                // Only a request parses as a prompt: a `session/prompt` without an id is no prompt at all.
                if let (AcpFrame::Prompt(prompt), Some(id)) = (message.body(), env.session())
                    && !env.frame().is_continuation()
                    && let Some(entry) = self.sessions.get_mut(id)
                    && !entry.prompted
                {
                    entry.prompted = true;
                    entry.auto_name = naming::auto_name(&prompt_text(prompt));
                }
                return;
            }
            _ => return,
        };
        let Some(id) = env.session() else {
            if let ShepherdFrame::WorkspaceSelected(selected) = message.body() {
                let repo = PathBuf::from(&selected.repo);
                self.workspaces.retain(|w| *w != repo);
                self.workspaces.insert(0, repo);
            }
            return;
        };
        match message.body() {
            ShepherdFrame::SessionCreated(created) => {
                if self.sessions.contains_key(id) {
                    return;
                }
                // A session given a branch but no worktree is waiting for its worktree to be made.
                let phase = match (&created.branch, &created.worktree) {
                    (Some(_), None) => Phase::Starting,
                    _ => Phase::Running,
                };
                self.branches.extend(created.branch.iter().cloned());
                let entry = Entry {
                    name: created.name.clone(),
                    created: env.ts().clone(),
                    phase,
                    closed: false,
                    agent: created.agent.as_deref().and_then(AgentKind::parse),
                    profile: created.profile.clone(),
                    cwd: created.cwd.as_ref().map(PathBuf::from),
                    agent_session_id: None,
                    workspace: created.workspace.as_ref().map(PathBuf::from),
                    warning: None,
                    renamed: false,
                    prompted: false,
                    auto_name: None,
                    // A session started in an existing worktree has it from the start.
                    worktree: created
                        .worktree
                        .as_ref()
                        .map(PathBuf::from)
                        .zip(created.branch.clone()),
                    branch: created.branch.clone(),
                    removing: false,
                };
                self.sessions.insert(id.clone(), entry);
            }
            ShepherdFrame::SessionRenamed(renamed) => {
                if let Some(entry) = self.sessions.get_mut(id).filter(|e| !e.closed) {
                    entry.name = renamed.name.clone();
                    entry.renamed = true;
                }
            }
            ShepherdFrame::SessionClosed(_) => {
                if let Some(entry) = self.sessions.get_mut(id) {
                    entry.closed = true;
                }
            }
            ShepherdFrame::WorktreeCreated(worktree) => {
                self.branches.insert(worktree.branch.clone());
                if let Some(entry) = self.sessions.get_mut(id)
                    && entry.phase == Phase::Starting
                {
                    entry.phase = match worktree.post_create.len() {
                        0 => Phase::Running,
                        left => Phase::Hooks { left },
                    };
                    entry.cwd = Some(PathBuf::from(&worktree.path));
                    entry.worktree = Some((PathBuf::from(&worktree.path), worktree.branch.clone()));
                }
            }
            ShepherdFrame::WorktreeRemoving(_) => {
                if let Some(entry) = self.sessions.get_mut(id)
                    && !entry.closed
                    && entry.removable_worktree().is_some()
                {
                    entry.removing = true;
                }
            }
            ShepherdFrame::WorktreeHook(hook) => {
                if let Some(entry) = self.sessions.get_mut(id)
                    && let Phase::Hooks { left } = entry.phase
                {
                    let failure = hook_failure(hook);
                    entry.phase = match left {
                        2.. if failure.is_none() => Phase::Hooks { left: left - 1 },
                        _ => Phase::Running,
                    };
                    if let Some(why) = failure {
                        entry.warning = Some(format!("hook failed ({why}): {}", hook.hook));
                    }
                }
            }
            ShepherdFrame::SessionFailed(failed) => {
                if let Some(entry) = self.sessions.get_mut(id)
                    && matches!(entry.phase, Phase::Starting | Phase::Hooks { .. })
                {
                    entry.phase = Phase::Failed(failed.reason.clone());
                }
            }
            ShepherdFrame::AgentExited(_) if env.source().kind == SourceKind::Shepherd => {
                if let Some(entry) = self.sessions.get_mut(id)
                    && entry.phase == Phase::Running
                {
                    entry.phase = Phase::Exited;
                }
            }
            ShepherdFrame::AgentStarted(started) => {
                if let Some(entry) = self.sessions.get_mut(id) {
                    if env.source().kind == SourceKind::Shepherd && entry.phase == Phase::Exited {
                        entry.phase = Phase::Running;
                    }
                    if let Some(agent_session) = &started.agent_session_id {
                        entry.agent_session_id = Some(agent_session.clone());
                    }
                }
            }
            _ => {}
        }
    }

    /// The worktrees in `listed` (the current workspace's managed ones) that no session that isn't closed uses: as its
    /// worktree or cwd ([`Host::used_by`]), or, for a worktree still being made in this workspace, by the branch it was
    /// given. A path that isn't UTF-8 can't be a client's cwd, so it is left out.
    fn orphans(&self, listed: Vec<Worktree>) -> WorktreeListOk {
        let workspace = self.workspace();
        let being_made = |w: &Worktree| {
            self.sessions.values().any(|e| {
                let same_branch = match (&e.branch, &w.branch) {
                    (Some(theirs), Some(branch)) => theirs == branch.as_str(),
                    _ => false,
                };
                !e.closed
                    && e.phase == Phase::Starting
                    && e.workspace.as_deref() == workspace
                    && same_branch
            })
        };
        let worktrees = listed
            .into_iter()
            .filter(|w| self.used_by(None, w.path.as_path()).is_none() && !being_made(w))
            .filter_map(|w| {
                Some(WorktreeInfo {
                    path: w.path.as_path().to_str()?.to_owned(),
                    branch: w.branch.map(|b| b.as_str().to_owned()),
                })
            })
            .collect();
        WorktreeListOk { worktrees }
    }

    /// A session that isn't closed, other than `except`, that uses `path`: as its worktree, or as its cwd.
    fn used_by(&self, except: Option<&SessionId>, path: &Path) -> Option<&SessionId> {
        self.sessions
            .iter()
            .filter(|(id, e)| !e.closed && Some(*id) != except)
            .find(|(_, e)| {
                let worktree = e.worktree.as_ref().is_some_and(|(p, _)| p == path);
                worktree || e.cwd.as_deref() == Some(path)
            })
            .map(|(id, _)| id)
    }

    /// Why closing `session` keeps its worktree at `path`: another session still uses it.
    fn still_used(&self, session: &SessionId, path: &Path) -> Option<String> {
        self.used_by(Some(session), path)
            .map(|other| format!("it is still in use by {other}"))
    }

    /// A session that exists and isn't closed or closing.
    fn open(&self, id: &SessionId) -> Option<&Entry> {
        self.sessions.get(id).filter(|e| !e.closed && !e.removing)
    }

    /// A session that is closing, removing its worktree.
    fn removing(&self, id: &SessionId) -> bool {
        self.sessions
            .get(id)
            .is_some_and(|e| e.removing && !e.closed)
    }

    /// An open session whose PTY is running.
    fn running(&self, id: &SessionId) -> bool {
        self.open(id).is_some_and(|e| e.phase == Phase::Running)
    }

    /// An open session waiting for its worktree or its post-create hooks.
    fn starting(&self, id: &SessionId) -> bool {
        self.open(id)
            .is_some_and(|e| matches!(e.phase, Phase::Starting | Phase::Hooks { .. }))
    }
}

/// The environment a worktree hook runs with, besides the host's own.
fn hook_env(
    session: &SessionId,
    path: &str,
    branch: &str,
    workspace: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("SHEPHERD_SESSION".to_owned(), session.to_string()),
        ("SHEPHERD_WORKTREE".to_owned(), path.to_owned()),
        ("SHEPHERD_BRANCH".to_owned(), branch.to_owned()),
    ];
    if let Some(repo) = workspace.and_then(Path::to_str) {
        env.push(("SHEPHERD_REPO".to_owned(), repo.to_owned()));
    }
    env
}

/// How a worktree hook's result is recorded in `worktree/hook`.
fn hook_record(command: &CommandSpec, result: Result<CommandOutput, CommandError>) -> WorktreeHook {
    let (exit_code, output_tail, error) = match result {
        Ok(CommandOutput { exit, tail }) => {
            let tail = display_tail(&tail);
            match exit {
                CommandExit::Code(code) => (Some(code), tail, None),
                CommandExit::Signal(signal) => {
                    (None, tail, Some(format!("killed by signal {signal}")))
                }
                CommandExit::TimedOut => (
                    None,
                    tail,
                    Some(format!("timed out after {}s", command.timeout.as_secs())),
                ),
            }
        }
        Err(error) => (None, String::new(), Some(error.to_string())),
    };
    WorktreeHook {
        hook: command.script.clone(),
        exit_code,
        output_tail,
        error,
    }
}

/// Why a recorded hook failed, for the user, or `None` if it exited 0.
fn hook_failure(hook: &WorktreeHook) -> Option<String> {
    match (&hook.error, hook.exit_code) {
        (None, Some(0)) => None,
        (Some(error), _) => Some(error.clone()),
        (None, Some(code)) => Some(format!("exit {code}")),
        (None, None) => Some("no exit code".to_owned()),
    }
}

/// A port error that means the worktree is already gone: it says there's no repo, and nothing is at the path.
fn is_gone(error: &WorktreeError, exists: bool) -> bool {
    matches!(error, WorktreeError::NotARepo(_)) && !exists
}

/// The answer to a client's `_shepherd/worktree/status` from what inspecting the worktree at `path` found.
fn status_answer(
    path: &WorktreePath,
    result: Result<Inspection, WorktreeError>,
) -> Result<HostResult, WireError> {
    let status = match result {
        Ok(Inspection::Listed {
            status: Ok(status), ..
        }) => Some(status),
        Ok(Inspection::Listed {
            status: Err(error),
            exists,
        }) if is_gone(&error, exists) => None,
        Ok(Inspection::Unlisted { exists: false }) => None,
        Ok(Inspection::Unlisted { exists: true }) => {
            let why = format!("{path} isn't a worktree shepherd manages");
            return Err(WireError::InvalidParams(why));
        }
        Err(error)
        | Ok(Inspection::Listed {
            status: Err(error), ..
        }) => {
            return Err(WireError::InvalidParams(error.to_string()));
        }
    };
    Ok(HostResult::WorktreeStatus(WorktreeStatusOk { status }))
}

/// The `session/closed` that ends session `id`.
fn closed(id: &SessionId, worktree_removed: bool, worktree_kept: Option<String>) -> HostCommand {
    let frame = ShepherdFrame::SessionClosed(SessionClosed {
        worktree_removed,
        worktree_kept,
    });
    append(id, frame)
}

/// A hook's output tail as recorded: decoded lossily, with control characters other than newlines and tabs removed.
/// A full tail ([`OUTPUT_TAIL`]) was cut from longer output, so a character it starts in the middle of is dropped.
fn display_tail(tail: &[u8]) -> String {
    let tail = match tail.len() {
        OUTPUT_TAIL => {
            let partial = tail
                .iter()
                .take(3)
                .take_while(|&&b| b & 0b1100_0000 == 0b1000_0000)
                .count();
            &tail[partial..]
        }
        _ => tail,
    };
    String::from_utf8_lossy(tail)
        .chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
        .collect()
}

/// The text blocks of a prompt, joined by newlines.
fn prompt_text(prompt: &PromptRequest) -> String {
    let text: Vec<&str> = prompt
        .prompt
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            ContentBlock::Other(_) => None,
        })
        .collect();
    text.join("\n")
}

/// A workspace path as the user typed it, with a leading `~` (alone, or followed by `/`) replaced by `home`, the
/// host's home directory. `~name` (another user's home) and every other path are returned as typed.
pub fn expand_home(typed: &Path, home: &Path) -> PathBuf {
    // Compares whole components, so `~name` doesn't match.
    match typed.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => typed.to_path_buf(),
    }
}

/// An `Append` of a frame the host produces about session `id`.
fn append(id: &SessionId, frame: ShepherdFrame) -> HostCommand {
    HostCommand::Append(Box::new(FrameDraft::new(
        frame.to_frame(),
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        Some(id.clone()),
    )))
}

/// An `Append` of a frame the host produces about itself, to the host log.
fn append_host(frame: ShepherdFrame) -> HostCommand {
    HostCommand::Append(Box::new(FrameDraft::new(
        frame.to_frame(),
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    )))
}

fn ack(to: ReplyTo) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Ok(HostResult::Ack),
    }
}

fn reply_err(to: ReplyTo, error: WireError) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Err(error),
    }
}

fn unknown_session(to: ReplyTo, session: SessionId) -> HostCommand {
    reply_err(to, WireError::UnknownSession { session })
}
