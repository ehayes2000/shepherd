//! The index's typed values: every id, hash and enum is validated when it is made and again when it is read back from
//! the index, so no free string is ever compared as text (decision 0020, T39).

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

/// Text that is not a valid value of some kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a valid {kind}: {reason}")]
pub struct BadValue {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

impl BadValue {
    fn new(kind: &'static str, value: &str, reason: &'static str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
            reason,
        }
    }
}

/// The longest label-like id the index accepts, in bytes.
const MAX_LABEL_LEN: usize = 1024;

/// An opaque id or name: non-empty, at most [`MAX_LABEL_LEN`] bytes, no control characters.
fn check_label(kind: &'static str, s: &str) -> Result<(), BadValue> {
    if s.is_empty() {
        return Err(BadValue::new(kind, s, "it is empty"));
    }
    if s.len() > MAX_LABEL_LEN {
        return Err(BadValue::new(kind, s, "it is longer than 1024 bytes"));
    }
    if s.chars().any(char::is_control) {
        return Err(BadValue::new(kind, s, "it contains a control character"));
    }
    Ok(())
}

/// A git object name: 40 (sha1) or 64 (sha256) lowercase hex digits. One spelling per object, so equal values are
/// equal names.
fn check_object_name(kind: &'static str, s: &str) -> Result<(), BadValue> {
    if !matches!(s.len(), 40 | 64) || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(BadValue::new(
            kind,
            s,
            "it must be 40 or 64 lowercase hex digits",
        ));
    }
    Ok(())
}

macro_rules! text_id {
    ($(#[$doc:meta])* $t:ident, $kind:literal, $check:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $t(String);

        impl $t {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $t {
            type Err = BadValue;

            fn from_str(s: &str) -> Result<Self, BadValue> {
                $check($kind, s)?;
                Ok(Self(s.to_owned()))
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

text_id!(
    /// A git commit hash.
    CommitHash,
    "commit hash",
    check_object_name
);
text_id!(
    /// A `git patch-id` (stable): the same change has the same patch-id after a rebase.
    PatchId,
    "patch id",
    check_object_name
);
text_id!(
    /// A shepherd session id. Opaque.
    SessionId,
    "session id",
    check_label
);
text_id!(
    /// The agent's own id for its session (what its resume flag takes). Opaque.
    AgentSessionId,
    "agent session id",
    check_label
);
text_id!(
    /// The machine a session ran on. Opaque.
    MachineId,
    "machine id",
    check_label
);
text_id!(
    /// A git branch name, as the session's worktree had it checked out.
    Branch,
    "branch",
    check_label
);

/// A repository, named by the hash of its root commit: the same in every clone, on every machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId(CommitHash);

impl RepoId {
    pub fn from_root_commit(root: CommitHash) -> Self {
        Self(root)
    }

    pub fn root_commit(&self) -> &CommitHash {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl FromStr for RepoId {
    type Err = BadValue;

    fn from_str(s: &str) -> Result<Self, BadValue> {
        check_object_name("repo id", s)?;
        Ok(Self(CommitHash(s.to_owned())))
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A pull request number in its repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrNumber(NonZeroU32);

impl PrNumber {
    pub fn new(n: NonZeroU32) -> Self {
        Self(n)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for PrNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A snapshot's row id in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(i64);

impl SnapshotId {
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! text_enum {
    ($(#[$doc:meta])* $t:ident, $kind:literal, { $($(#[$vdoc:meta])* $v:ident => $s:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $t {
            $($(#[$vdoc])* $v),+
        }

        impl $t {
            /// The name the index stores.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$v => $s),+
                }
            }
        }

        impl FromStr for $t {
            type Err = BadValue;

            fn from_str(s: &str) -> Result<Self, BadValue> {
                match s {
                    $($s => Ok(Self::$v),)+
                    _ => Err(BadValue::new($kind, s, "unknown name")),
                }
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(
    /// Which coding agent ran a session.
    AgentKind, "agent", {
        Claude => "claude",
        Codex => "codex",
    }
);
text_enum!(
    /// Why a snapshot was taken.
    SnapshotReason, "snapshot reason", {
        /// The agent finished a turn.
        TurnEnd => "turn_end",
        /// The session was closed.
        Close => "close",
        /// Its worktree was about to be removed.
        BeforeRemove => "before_remove",
        /// Someone asked for it.
        Manual => "manual",
    }
);
text_enum!(
    /// What a file in a snapshot is.
    FileRole, "file role", {
        /// Claude Code's main session transcript.
        Main => "main",
        /// A Claude Code subagent transcript.
        Subagent => "subagent",
        /// A Codex rollout file.
        Rollout => "rollout",
    }
);
