//! The ACP++ logs: one for the host, one per session (see `docs/acpp.md`). The log assigns each entry its
//! `seq` and seals the draft; everything else in the envelope comes from the caller's [`Stamp`].
//!
//! The trait is synchronous because an append is one buffered write. It takes `&mut self` for appends, so a
//! log has a single writer: if several tasks need to append, the caller serializes them (the host owns the log).

use std::fmt;

use crate::acpp::{Envelope, FrameDraft, Seq, Stamp};
use crate::ids::SessionId;

/// Which log an entry belongs to. Host entries have no session; a session's entries carry its id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogId {
    Host,
    Session(SessionId),
}

impl LogId {
    /// The log a draft must be appended to.
    pub fn of(draft: &FrameDraft) -> Self {
        match &draft.session {
            None => Self::Host,
            Some(id) => Self::Session(id.clone()),
        }
    }

    /// The session entries of this log carry: `None` for the host log.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Host => None,
            Self::Session(id) => Some(id),
        }
    }

    /// Rejects session ids that can't safely name a log on disk: empty, starting with `.` (so `.` and `..`),
    /// or containing a path separator or NUL.
    pub fn validate(&self) -> Result<(), LogError> {
        match self {
            Self::Host => Ok(()),
            Self::Session(id) => {
                let s = id.as_str();
                let unsafe_name =
                    s.is_empty() || s.starts_with('.') || s.contains(['/', '\\', '\0']);
                if unsafe_name {
                    Err(LogError::InvalidSessionId(id.clone()))
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl fmt::Display for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => f.write_str("host log"),
            Self::Session(id) => write!(f, "session log {id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogError {
    /// The draft's session doesn't match the log it was appended to.
    #[error("draft for session {draft:?} appended to {log}")]
    WrongLog {
        log: LogId,
        draft: Option<SessionId>,
    },
    /// The session id can't name a log (see [`LogId::validate`]).
    #[error("session id {0:?} is not a valid log name")]
    InvalidSessionId(SessionId),
    /// A stored entry before the last one is unreadable, or the seqs aren't gapless. `line` counts from 1.
    #[error("{log} is corrupt at line {line}: {reason}")]
    Corrupt {
        log: LogId,
        line: u64,
        reason: String,
    },
    #[error("log io: {0}")]
    Io(String),
}

pub trait EventLog {
    /// Seals `draft` with the next seq of `log` (1 for an empty log) and stores it. The draft's session must
    /// match `log`, otherwise [`LogError::WrongLog`]. A failed append stores nothing and uses up no seq.
    fn append(&mut self, log: LogId, draft: FrameDraft, stamp: Stamp)
    -> Result<Envelope, LogError>;

    /// The entries of `log` with a seq greater than `from`, in seq order. `from` is the last seq the reader
    /// already has, so [`Seq::ZERO`] reads the whole log. A log with no entries reads as empty.
    fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, LogError>;

    /// Every log with at least one entry, sorted (the host log first, then sessions by id).
    fn logs(&self) -> Result<Vec<LogId>, LogError>;
}
