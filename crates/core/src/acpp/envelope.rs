//! The ACP++ envelope and the draft producers create before the host appends it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::Frame;
use crate::ids::{MachineId, SessionId};

/// The envelope version this code reads and writes. It only bumps for breaking envelope changes, so an envelope
/// with any other version is rejected.
pub const ENVELOPE_VERSION: u32 = 1;

/// One log entry: a frame plus what the log knew when it appended it. Unknown envelope fields are kept and
/// serialized back.
///
/// Envelopes come from reading a log, or from [`FrameDraft::seal`] on the append path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "EnvelopeRepr")]
pub struct Envelope {
    acpp: u32,
    seq: Seq,
    id: EntryId,
    ts: Timestamp,
    machine: MachineId,
    session: Option<SessionId>,
    source: Source,
    dir: Dir,
    frame: Frame,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// The wire shape of an envelope, before its version is checked.
#[derive(Deserialize)]
struct EnvelopeRepr {
    acpp: u64,
    seq: Seq,
    id: EntryId,
    ts: Timestamp,
    machine: MachineId,
    session: Option<SessionId>,
    source: Source,
    dir: Dir,
    frame: Frame,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl TryFrom<EnvelopeRepr> for Envelope {
    type Error = EnvelopeError;

    fn try_from(r: EnvelopeRepr) -> Result<Self, Self::Error> {
        if r.acpp != u64::from(ENVELOPE_VERSION) {
            return Err(EnvelopeError::UnsupportedVersion(r.acpp));
        }
        Ok(Self {
            acpp: ENVELOPE_VERSION,
            seq: r.seq,
            id: r.id,
            ts: r.ts,
            machine: r.machine,
            session: r.session,
            source: r.source,
            dir: r.dir,
            frame: r.frame,
            extra: r.extra,
        })
    }
}

/// Why a JSON value is not an envelope this version can read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("unsupported envelope version {0}, expected {ENVELOPE_VERSION}")]
    UnsupportedVersion(u64),
    /// A missing or ill-typed field, a bad timestamp, or a malformed frame.
    #[error("malformed envelope: {0}")]
    Malformed(String),
}

impl Envelope {
    /// Parses an envelope, reporting why it can't be read.
    pub fn from_value(raw: Value) -> Result<Self, EnvelopeError> {
        let repr: EnvelopeRepr =
            serde_json::from_value(raw).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
        Self::try_from(repr)
    }

    pub fn acpp(&self) -> u32 {
        self.acpp
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn id(&self) -> &EntryId {
        &self.id
    }

    pub fn ts(&self) -> &Timestamp {
        &self.ts
    }

    pub fn machine(&self) -> &MachineId {
        &self.machine
    }

    /// `None` for host-level entries.
    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn dir(&self) -> &Dir {
        &self.dir
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Envelope fields this version doesn't know.
    pub fn extra(&self) -> &Map<String, Value> {
        &self.extra
    }
}

/// An entry's position in its log: starts at 1, gapless. Assigned by the log at append time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// Before the first entry. Reading a log from here returns all of it.
    pub const ZERO: Seq = Seq(0);

    pub fn new(n: u64) -> Self {
        Self(n)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The fields the host assigns to every entry it appends, apart from `seq`, which the log assigns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub id: EntryId,
    pub ts: Timestamp,
    pub machine: MachineId,
}

/// What a producer creates: a frame and what it knows about it. The host's append path seals it into an
/// [`Envelope`]. Serializable, because producers like `shepherd hook` send drafts to the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameDraft {
    pub frame: Frame,
    pub source: Source,
    pub dir: Dir,
    pub session: Option<SessionId>,
}

impl FrameDraft {
    pub fn new(frame: Frame, source: Source, dir: Dir, session: Option<SessionId>) -> Self {
        Self {
            frame,
            source,
            dir,
            session,
        }
    }

    /// Seals the draft with the fields assigned at append time.
    pub fn seal(self, seq: Seq, stamp: Stamp) -> Envelope {
        Envelope {
            acpp: ENVELOPE_VERSION,
            seq,
            id: stamp.id,
            ts: stamp.ts,
            machine: stamp.machine,
            session: self.session,
            source: self.source,
            dir: self.dir,
            frame: self.frame,
            extra: Map::new(),
        }
    }
}

/// Who produced a frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    /// The agent, when there is one (`claude-code`, `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Source fields this version doesn't know.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Source {
    pub fn new(kind: SourceKind, agent: Option<String>) -> Self {
        Self {
            kind,
            agent,
            extra: Map::new(),
        }
    }
}

open_enum! {
    pub enum SourceKind {
        /// A real ACP wire.
        Acp = "acp",
        /// Translated from agent hooks.
        Hook = "hook",
        /// The host itself.
        Shepherd = "shepherd",
        /// A third-party producer.
        Ext = "ext",
    }
}

open_enum! {
    /// The ACP direction of a frame.
    pub enum Dir {
        ToAgent = "to_agent",
        FromAgent = "from_agent",
        Internal = "internal",
    }
}

impl Dir {
    /// The direction a response to a frame travelling this way takes.
    pub fn reversed(&self) -> Dir {
        match self {
            Self::ToAgent => Self::FromAgent,
            Self::FromAgent => Self::ToAgent,
            other => other.clone(),
        }
    }
}

/// A log entry's globally unique id (a ULID, generated by the runtime). Opaque to core.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(String);

impl EntryId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An RFC 3339 timestamp, kept as the exact string it was written as. Validated on construction, so a
/// `Timestamp` is always well-formed. Core never reads the clock: the runtime formats the time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Timestamp(String);

/// The string is not an RFC 3339 date-time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an RFC 3339 timestamp: {0:?}")]
pub struct TimestampError(String);

impl Timestamp {
    pub fn parse(s: impl Into<String>) -> Result<Self, TimestampError> {
        let s = s.into();
        if is_rfc3339(s.as_bytes()) {
            Ok(Self(s))
        } else {
            Err(TimestampError(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// RFC 3339 `date-time`: `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`, with `T` and `Z` in either case.
fn is_rfc3339(b: &[u8]) -> bool {
    fn num(b: &[u8], at: usize, len: usize) -> Option<u32> {
        let digits = b.get(at..at + len)?;
        digits.iter().try_fold(0u32, |acc, d| {
            d.is_ascii_digit().then(|| acc * 10 + u32::from(d - b'0'))
        })
    }
    let sep = |at: usize, c: u8| b.get(at) == Some(&c);
    let parsed = (|| {
        let (year, month, day) = (num(b, 0, 4)?, num(b, 5, 2)?, num(b, 8, 2)?);
        let (hour, minute, second) = (num(b, 11, 2)?, num(b, 14, 2)?, num(b, 17, 2)?);
        let shape = sep(4, b'-')
            && sep(7, b'-')
            && matches!(b.get(10), Some(b'T' | b't'))
            && sep(13, b':')
            && sep(16, b':');
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let in_range = (1..=12).contains(&month)
            && (1..=days).contains(&day)
            && hour <= 23
            && minute <= 59
            && second <= 60;

        let mut at = 19;
        if sep(at, b'.') {
            let digits = b[at + 1..]
                .iter()
                .take_while(|d| d.is_ascii_digit())
                .count();
            if digits == 0 {
                return None;
            }
            at += 1 + digits;
        }
        let offset = match b.get(at)? {
            b'Z' | b'z' => at + 1 == b.len(),
            b'+' | b'-' => {
                let (oh, om) = (num(b, at + 1, 2)?, num(b, at + 4, 2)?);
                sep(at + 3, b':') && oh <= 23 && om <= 59 && at + 6 == b.len()
            }
            _ => false,
        };
        Some(shape && in_range && offset)
    })();
    parsed.unwrap_or(false)
}
