//! The ACP++ logs: one for the host, one per session (see `docs/acpp.md`). The log assigns each entry its
//! `seq` and seals the draft; everything else in the envelope comes from the caller's [`Stamp`].
//!
//! The trait is synchronous because an append is a single write. It takes `&mut self` for appends, so a log has
//! a single writer: if several tasks need to append, the caller serializes them (the host owns the log).
//!
//! Durability (decision 0009): once `append` returns `Ok`, the entry survives a crash of this process. It may
//! not survive power loss or a kernel panic, because there is no fsync per append. Implementations clean up
//! what such a loss leaves at the end of a log when they open it.

use std::fmt;
use std::path::PathBuf;

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

    /// Rejects session ids that can't name a log file. An id must be 1 to 64 characters of `[a-z0-9_-]`.
    /// Lowercase only, so two ids can't alias on a case-insensitive filesystem. The runtime generates lowercase
    /// `s_<ulid>` ids.
    pub fn validate(&self) -> Result<(), LogError> {
        match self {
            Self::Host => Ok(()),
            Self::Session(id) => {
                let s = id.as_str();
                let allowed = |c: char| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-');
                if (1..=64).contains(&s.len()) && s.chars().all(allowed) {
                    Ok(())
                } else {
                    Err(LogError::InvalidSessionId(id.clone()))
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
    /// A complete stored entry is unreadable, the seqs aren't gapless, or an entry belongs to another log.
    /// `line` counts from 1.
    #[error("{log} is corrupt at line {line}: {reason}")]
    Corrupt {
        log: LogId,
        line: u64,
        reason: String,
    },
    /// Another process has the log store open. A store has a single writer.
    #[error("the log store is locked by another process")]
    Locked,
    /// A failed append couldn't be undone, so where the log ends is unknown. Appends to it fail until the
    /// store is reopened, which recovers it.
    #[error("{log} is poisoned by a failed append; reopen to recover")]
    Poisoned { log: LogId },
    /// The stored log isn't the length this store left it at, so something else changed it. Nothing was
    /// written.
    #[error("{log} changed on disk: expected {expected} bytes, found {found}")]
    ChangedOnDisk {
        log: LogId,
        expected: u64,
        found: u64,
    },
    /// A store directory or file is accessible to group or others (decision 0010). It is never chmodded: the
    /// user fixes it.
    #[error("{path} has mode {mode:o}; the log store must not be accessible to group or others")]
    InsecurePermissions { path: PathBuf, mode: u32 },
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
