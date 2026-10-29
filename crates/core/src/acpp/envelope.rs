//! The ACP++ envelope and the draft producers create before the host appends it.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

/// An ISO 8601 instant as `jiff` parses it: RFC 3339, plus forms such as the basic format, reduced precision and
/// RFC 9557 bracketed annotations, as long as an offset or `Z` fixes the instant. Fractions longer than 9 digits are
/// rejected. Writers emit RFC 3339 UTC with milliseconds (`2026-10-29T12:00:00.000Z`). Parsed on construction, so a
/// `Timestamp` is always a valid instant. Core never reads the clock: the runtime formats the time.
///
/// It keeps the exact string it was parsed from: [`Timestamp::as_str`], `Display` and serialization all give back
/// that original text, so an envelope round-trips unchanged. Equality, ordering and hashing are by **instant**:
/// `12:30:00+01:00` and `11:30:00.000Z` are equal, though their strings differ.
#[derive(Debug, Clone)]
pub struct Timestamp {
    raw: String,
    instant: jiff::Timestamp,
}

/// The string is not an ISO 8601 instant that `jiff` can parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an ISO 8601 instant (date, time and offset): {0:?}")]
pub struct TimestampError(String);

impl Timestamp {
    pub fn parse(s: impl Into<String>) -> Result<Self, TimestampError> {
        let raw = s.into();
        match raw.parse::<jiff::Timestamp>() {
            Ok(instant) => Ok(Self { raw, instant }),
            Err(_) => Err(TimestampError(raw)),
        }
    }

    /// The original text, exactly as parsed.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The instant it names.
    pub fn instant(&self) -> jiff::Timestamp {
        self.instant
    }
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.instant == other.instant
    }
}

impl Eq for Timestamp {}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.instant.cmp(&other.instant)
    }
}

impl Hash for Timestamp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.instant.hash(state);
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
        f.write_str(&self.raw)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
