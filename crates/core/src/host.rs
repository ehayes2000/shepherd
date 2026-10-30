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
//! paths as the probe resolved them, compared as paths. Each `session/created` records the workspace it was created in.
//!
//! Worktrees: a new session in a workspace, unless the request named its own cwd, gets a worktree of the workspace's
//! repo, on a branch named after the session ([`naming`]) and started from the origin default. Its `session/created`
//! records the branch and no cwd, and it is listed at once, waiting ([`ResumeUnavailable::Starting`], no PTY), while
//! [`HostCommand::CreateWorktree`] runs off the runtime thread. The result comes back as [`HostEvent::WorktreeCreated`]:
//! the worktree is recorded in `worktree/created`, whose path is the session's cwd from then on (resume uses it), and
//! the PTY spawns there. A failure is recorded as `session/failed`, and the session stays listed with the reason and no
//! PTY. A result for a session closed meanwhile is recorded the same way but spawns nothing; a created worktree is left
//! in place for cleanup to find through its record.
//!
//! Host state is only a fold over `_shepherd/*` frames (`docs/acpp.md`, "State is a fold"). [`Host::handle`] reads
//! state and decides what to do. It never changes state: every change is a [`HostCommand::Append`] that the runtime
//! seals, appends, and hands back to [`Host::apply`].

pub mod launch;
pub mod naming;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::acpp::{
    AgentExited, AgentStarted, Dir, Envelope, Frame, FrameDraft, SessionClosed, SessionCreated,
    SessionFailed, SessionRenamed, ShepherdFrame, Source, SourceKind, Timestamp, WorkspaceSelected,
    WorktreeCreated,
};
use crate::agents::Translators;
use crate::config::AgentKind;
use crate::ids::SessionId;
use crate::ports::pty::{PtySize, SpawnSpec};
use crate::ports::worktrees::{BaseSpec, BaseUsed, Created, RepoRef, WorktreeError, WorktreeName};
use crate::wire::{HostResult, SessionNewOk, WireError};

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
    pub spawn: SpawnSpec,
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
    Close {
        reply: ReplyTo,
        session: SessionId,
    },
    /// Bytes a client typed into a session. A notification: never answered.
    Input {
        session: SessionId,
        bytes: Vec<u8>,
    },
    /// A client resized a session's terminal. A notification: never answered.
    Resize {
        session: SessionId,
        size: PtySize,
    },
    /// `shepherd hook` sent a hook payload from an agent running in `session`.
    Hook {
        reply: ReplyTo,
        session: SessionId,
        agent: AgentKind,
        payload: Value,
    },
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
    SelectWorkspace {
        ask: WorkspaceAsk,
        path: PathBuf,
    },
    /// The answer to a [`HostCommand::ProbeWorkspace`]: the repo the path resolved to, or why it isn't one, for the
    /// user.
    WorkspaceProbed {
        ask: WorkspaceAsk,
        repo: Result<PathBuf, String>,
    },
    /// The answer to a [`HostCommand::CreateWorktree`], with the `spawn` it carried.
    WorktreeCreated {
        session: SessionId,
        spawn: SpawnSpec,
        result: Result<Created, WorktreeError>,
    },
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
    WritePty {
        session: SessionId,
        bytes: Vec<u8>,
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
    /// Find out whether `path` is in a git repo, resolving `..` and symlinks, and feed the answer back as
    /// [`HostEvent::WorkspaceProbed`] with the same `ask`. The probe runs off the runtime thread, so other events may
    /// be handled before its answer arrives, and the answer may no longer matter by then.
    ProbeWorkspace {
        ask: WorkspaceAsk,
        path: PathBuf,
    },
    /// Create worktree `name` of `repo` from `base`, and feed the result back as [`HostEvent::WorktreeCreated`] with
    /// the same `session` and `spawn`: what to spawn once the worktree exists (its cwd is replaced by the worktree's
    /// path). Runs off the runtime thread, like [`HostCommand::ProbeWorkspace`].
    CreateWorktree {
        session: SessionId,
        repo: RepoRef,
        name: WorktreeName,
        base: BaseSpec,
        spawn: SpawnSpec,
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
    /// It is waiting for its worktree to be made, and has no PTY yet.
    pub creating_worktree: bool,
}

/// Where a session is in its life, as far as its PTY goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Waiting for its worktree. No PTY yet.
    Starting,
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
}

impl Entry {
    fn resume_target(&self) -> Result<ResumeTarget, ResumeUnavailable> {
        match self.phase {
            Phase::Starting => return Err(ResumeUnavailable::Starting),
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
                exited: matches!(e.phase, Phase::Exited | Phase::Failed(_)),
                resume: e.resume_target().map(|_| ()),
                workspace: e.workspace.clone(),
                failed: match &e.phase {
                    Phase::Failed(reason) => Some(reason.clone()),
                    _ => None,
                },
                creating_worktree: e.phase == Phase::Starting,
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
                let worktree = workspace.filter(|_| !spec.explicit_cwd).map(|repo| {
                    (
                        RepoRef::new(repo),
                        naming::worktree_name(&name, &self.branches),
                    )
                });
                let created = ShepherdFrame::SessionCreated(SessionCreated {
                    name,
                    workspace: workspace.map(str::to_owned),
                    worktree: None,
                    branch: worktree.as_ref().map(|(_, name)| name.as_str().to_owned()),
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
            HostEvent::Close { reply, session } => {
                let Some(entry) = self.open(&session) else {
                    return vec![unknown_session(reply, session)];
                };
                let mut commands = Vec::new();
                if entry.phase == Phase::Running {
                    commands.push(HostCommand::KillPty {
                        session: session.clone(),
                    });
                }
                let closed = ShepherdFrame::SessionClosed(SessionClosed {
                    worktree_removed: false,
                });
                commands.push(append(&session, closed));
                commands.push(ack(reply));
                commands
            }
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
                let failed = ShepherdFrame::SessionFailed(SessionFailed {
                    reason: "the host stopped before the worktree was ready".to_owned(),
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
            HostEvent::Input { session, bytes } => vec![HostCommand::WritePty { session, bytes }],
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
                result,
            } => self.worktree_created(session, spawn, result),
        }
    }

    /// Records what came of a session's worktree, and spawns its PTY there if the session is still open. Only a
    /// session waiting for its worktree takes a result.
    fn worktree_created(
        &self,
        session: SessionId,
        mut spawn: SpawnSpec,
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
            path,
            branch: created.branch.to_string(),
            base,
            fetch_error,
        });
        let mut commands = vec![append(&session, recorded)];
        if !entry.closed {
            spawn.cwd = created.path.as_path().to_path_buf();
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

    /// Folds one log entry into the state. Entries the registry doesn't read change nothing, and no entry panics.
    ///
    /// Only the host's own `agent/exited` (source `shepherd`) marks a PTY exited: one from a hook is about an agent
    /// the shell ran, and the shell is still running. Likewise only the host's own `agent/started` (a resume) marks
    /// it running again. An `agent/started` from any source records its agent session id.
    pub fn apply(&mut self, env: &Envelope) {
        let Frame::Shepherd(message) = env.frame() else {
            return;
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
                };
                self.sessions.insert(id.clone(), entry);
            }
            ShepherdFrame::SessionRenamed(renamed) => {
                if let Some(entry) = self.sessions.get_mut(id).filter(|e| !e.closed) {
                    entry.name = renamed.name.clone();
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
                    entry.phase = Phase::Running;
                    entry.cwd = Some(PathBuf::from(&worktree.path));
                }
            }
            ShepherdFrame::SessionFailed(failed) => {
                if let Some(entry) = self.sessions.get_mut(id)
                    && entry.phase == Phase::Starting
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

    /// A session that exists and isn't closed.
    fn open(&self, id: &SessionId) -> Option<&Entry> {
        self.sessions.get(id).filter(|e| !e.closed)
    }

    /// An open session whose PTY is running.
    fn running(&self, id: &SessionId) -> bool {
        self.open(id).is_some_and(|e| e.phase == Phase::Running)
    }

    /// An open session waiting for its worktree.
    fn starting(&self, id: &SessionId) -> bool {
        self.open(id).is_some_and(|e| e.phase == Phase::Starting)
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
