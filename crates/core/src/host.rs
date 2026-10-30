//! The host's session registry: which sessions exist, their names, and whether their PTY has exited.
//!
//! Host state is only a fold over `_shepherd/*` frames (`docs/acpp.md`, "State is a fold"). [`Host::handle`] reads
//! state and decides what to do. It never changes state: every change is a [`HostCommand::Append`] that the runtime
//! seals, appends, and hands back to [`Host::apply`].

pub mod launch;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::acpp::{
    AgentExited, Dir, Envelope, Frame, FrameDraft, SessionClosed, SessionCreated, SessionRenamed,
    ShepherdFrame, Source, SourceKind, Timestamp,
};
use crate::agents::Translators;
use crate::config::AgentKind;
use crate::ids::SessionId;
use crate::ports::pty::{PtySize, SpawnSpec};
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

/// What to start a new session with. The runtime fills it in: in M1 `spawn.command` is the user's `$SHELL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionSpec {
    /// The session's name. `None` gives it a placeholder, `shell-<n>`.
    pub name: Option<String>,
    pub spawn: SpawnSpec,
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
}

/// A session as [`Host::sessions`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSession {
    pub id: SessionId,
    pub name: String,
    /// The `ts` of its `session/created` entry.
    pub created: Timestamp,
    /// Its PTY process has ended.
    pub exited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    /// With the session id, orders [`Host::sessions`].
    created: Timestamp,
    exited: bool,
    closed: bool,
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
/// - If an [`HostCommand::Append`] fails, the runtime skips the rest of that `handle` call's commands. If they
///   include a [`HostCommand::Reply`], it sends that reply as `Err(WireError::Internal)` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Host {
    /// Every session ever created, closed ones included, so a closed id stays unknown to requests and still counts
    /// towards placeholder names.
    sessions: BTreeMap<SessionId, Entry>,
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
                exited: e.exited,
            })
            .collect()
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
                    .unwrap_or_else(|| format!("shell-{}", self.sessions.len() + 1));
                let created = ShepherdFrame::SessionCreated(SessionCreated {
                    name,
                    workspace: None,
                    worktree: None,
                    branch: None,
                    agent: None,
                    machine: None,
                });
                vec![
                    append(&id, created),
                    HostCommand::SpawnPty {
                        session: id.clone(),
                        spec: spec.spawn,
                    },
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
                if !entry.exited {
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
        }
    }

    /// Folds one log entry into the state. Entries the registry doesn't read change nothing, and no entry panics.
    ///
    /// Only the host's own `agent/exited` (source `shepherd`) marks a PTY exited: one from a hook is about an agent
    /// the shell ran, and the shell is still running.
    pub fn apply(&mut self, env: &Envelope) {
        let (Some(id), Frame::Shepherd(message)) = (env.session(), env.frame()) else {
            return;
        };
        match message.body() {
            ShepherdFrame::SessionCreated(created) => {
                if self.sessions.contains_key(id) {
                    return;
                }
                let entry = Entry {
                    name: created.name.clone(),
                    created: env.ts().clone(),
                    exited: false,
                    closed: false,
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
            ShepherdFrame::AgentExited(_) if env.source().kind == SourceKind::Shepherd => {
                if let Some(entry) = self.sessions.get_mut(id) {
                    entry.exited = true;
                }
            }
            _ => {}
        }
    }

    /// A session that exists and isn't closed.
    fn open(&self, id: &SessionId) -> Option<&Entry> {
        self.sessions.get(id).filter(|e| !e.closed)
    }

    /// An open session whose PTY hasn't exited.
    fn running(&self, id: &SessionId) -> bool {
        self.open(id).is_some_and(|e| !e.exited)
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
