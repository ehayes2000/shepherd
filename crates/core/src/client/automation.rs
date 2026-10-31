//! What scripts and agents drive shepherd with (T46): a session's status in four words, and naming a session (a
//! [`Target`]) or a machine by its id or its name. Every command that acts on a session resolves it here, so they all
//! read a name the same way.

use std::fmt;

use super::{Client, ClientSession, MachineGroup};
use crate::ids::{MachineId, SessionId};
use crate::session::Activity;

/// What a session is doing, as a script sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Its process runs and isn't known to be waiting: the agent is in a turn, still starting, or it runs no agent.
    Running,
    /// The agent is at its prompt with nothing to do.
    Idle,
    /// The agent is waiting on the user: it asked permission or a question.
    WantsUser,
    /// Its process ended, or it never started.
    Exited,
}

impl SessionStatus {
    pub fn of(session: &ClientSession) -> Self {
        if session.exited {
            return Self::Exited;
        }
        match session.activity {
            Activity::Exited { .. } => Self::Exited,
            Activity::NeedsPermission { .. } | Activity::NeedsInput => Self::WantsUser,
            Activity::Idle => Self::Idle,
            Activity::Starting | Activity::Working { .. } | Activity::Unknown => Self::Running,
        }
    }

    /// What `session`, listed by `client`, is doing: as [`SessionStatus::of`], except that an agent with a subagent
    /// waiting on the user wants the user too, and an agent which reported its start and hasn't had a turn yet is at
    /// its prompt, so idle ([`crate::session::SessionState::at_prompt`]).
    pub fn in_client(client: &Client, session: &ClientSession) -> Self {
        let status = Self::of(session);
        let state = client.state(&session.id);
        match status {
            Self::Exited => Self::Exited,
            _ if state.is_some_and(|s| s.wants_user()) => Self::WantsUser,
            Self::Running if state.is_some_and(|s| s.at_prompt()) => Self::Idle,
            other => other,
        }
    }

    /// How `docs/cli.md` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::WantsUser => "wants-user",
            Self::Exited => "exited",
        }
    }
}

/// A session as an error lists it: its id, its name, and what its machine is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: SessionId,
    pub name: String,
    pub machine: String,
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} on {})", self.name, self.id, self.machine)
    }
}

/// Why a [`Target`] names no one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// No session has that id or name. `candidates` is every session, newest first.
    Unknown {
        target: String,
        candidates: Vec<Candidate>,
    },
    /// More than one session has that name. `candidates` are those, newest first.
    Ambiguous {
        target: String,
        candidates: Vec<Candidate>,
    },
}

fn list(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(|c| format!("\n  {c}"))
        .collect::<String>()
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { target, candidates } if candidates.is_empty() => {
                write!(f, "no session is called `{target}`: there are no sessions")
            }
            Self::Unknown { target, candidates } => write!(
                f,
                "no session is called `{target}`; the sessions are:{}",
                list(candidates)
            ),
            Self::Ambiguous { target, candidates } => write!(
                f,
                "more than one session is called `{target}`; name one by its id:{}",
                list(candidates)
            ),
        }
    }
}

impl std::error::Error for TargetError {}

/// What a command names a session by: its id, or its name if no other session has it. An id wins over a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target(String);

impl Target {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The listed session this names, on any machine `client` reaches.
    pub fn resolve<'a>(&self, client: &'a Client) -> Result<&'a ClientSession, TargetError> {
        let sessions = newest_first(client);
        if let Some(session) = sessions.iter().find(|s| s.id.as_str() == self.0) {
            return Ok(session);
        }
        let named: Vec<&ClientSession> = sessions
            .iter()
            .copied()
            .filter(|s| s.name == self.0)
            .collect();
        match named[..] {
            [session] => Ok(session),
            [] => Err(TargetError::Unknown {
                target: self.0.clone(),
                candidates: candidates(client, &sessions),
            }),
            _ => Err(TargetError::Ambiguous {
                target: self.0.clone(),
                candidates: candidates(client, &named),
            }),
        }
    }
}

/// Every listed session, newest first across machines: by its creation, then by id.
pub fn newest_first(client: &Client) -> Vec<&ClientSession> {
    let mut sessions: Vec<&ClientSession> = client.sessions().iter().collect();
    sessions.sort_by(|a, b| (&b.created, &b.id).cmp(&(&a.created, &a.id)));
    sessions
}

fn candidates(client: &Client, sessions: &[&ClientSession]) -> Vec<Candidate> {
    sessions
        .iter()
        .map(|s| Candidate {
            id: s.id.clone(),
            name: s.name.clone(),
            machine: client.machine_label(&s.machine).to_owned(),
        })
        .collect()
}

/// Why text names no one machine. Each lists the machines as `(name, id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineError {
    /// No machine has that id or name. `known` is every machine the client knows of.
    Unknown {
        target: String,
        known: Vec<(String, MachineId)>,
    },
    /// More than one trusted machine has that name.
    Ambiguous {
        target: String,
        candidates: Vec<(String, MachineId)>,
    },
}

fn machine_list(machines: &[(String, MachineId)]) -> String {
    machines
        .iter()
        .map(|(name, id)| format!("\n  {name} ({id})"))
        .collect()
}

impl fmt::Display for MachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { target, known } => write!(
                f,
                "no machine is called `{target}`; the machines are:{}",
                machine_list(known)
            ),
            Self::Ambiguous { target, candidates } => write!(
                f,
                "more than one machine is called `{target}`; name one by its id:{}",
                machine_list(candidates)
            ),
        }
    }
}

impl std::error::Error for MachineError {}

/// The machine (this one or a trusted peer) whose id is `text`, else the one whose name is.
pub fn resolve_machine(client: &Client, text: &str) -> Result<MachineGroup, MachineError> {
    let machines = client.machines();
    if let Some(machine) = machines.iter().find(|m| m.id.as_str() == text) {
        return Ok(machine.clone());
    }
    let named: Vec<&MachineGroup> = machines.iter().filter(|m| m.name == text).collect();
    let listed = |ms: &[&MachineGroup]| -> Vec<(String, MachineId)> {
        ms.iter().map(|m| (m.name.clone(), m.id.clone())).collect()
    };
    match named[..] {
        [machine] => Ok(machine.clone()),
        [] => Err(MachineError::Unknown {
            target: text.to_owned(),
            known: listed(&machines.iter().collect::<Vec<_>>()),
        }),
        _ => Err(MachineError::Ambiguous {
            target: text.to_owned(),
            candidates: listed(&named),
        }),
    }
}
