//! ACP++: the append-only agent event log. See `docs/acpp.md`.
//!
//! An [`Envelope`] wraps exactly one JSON-RPC 2.0 [`Frame`]. Frames are lossless: each one keeps the raw JSON it
//! was parsed from and always serializes back to it. The typed parse is a read-only view next to the raw value.
//! Producers build frames from typed values ([`ShepherdFrame::to_frame`], the `to_frame` methods in [`acp`], and
//! [`Frame::notification`] for vendor frames), mark them with [`Frame::into_synthesized`] when they reconstruct
//! ACP, and wrap them in a [`FrameDraft`]. Only the host process's append path seals a draft into an envelope,
//! because only it knows `seq`, `id`, `ts` and `machine`.

/// A string enum that never fails to parse. Known wire values get a variant, anything else lands in
/// `Other(String)` and serializes back unchanged.
macro_rules! open_enum {
    (
        $(#[$attr:meta])*
        pub enum $name:ident { $($(#[$vattr:meta])* $variant:ident = $wire:literal,)* }
    ) => {
        $(#[$attr])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vattr])* $variant,)*
            /// A value this version doesn't know. Kept verbatim.
            Other(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)*
                    Self::Other(s) => s,
                }
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                match s {
                    $($wire => Self::$variant,)*
                    other => Self::Other(other.to_owned()),
                }
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                match Self::from(s.as_str()) {
                    Self::Other(_) => Self::Other(s),
                    known => known,
                }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d).map(Self::from)
            }
        }
    };
}

pub mod acp;
mod envelope;
mod frame;
mod shepherd;

pub use envelope::{
    Dir, ENVELOPE_VERSION, EntryId, Envelope, EnvelopeError, FrameDraft, Seq, Source, SourceKind,
    Stamp, Timestamp, TimestampError,
};
pub use frame::{Correlated, Frame, FrameError, Message, PendingRequests, RequestId, Response};
pub use shepherd::{
    AgentExited, AgentIdle, AgentStarted, SessionClosed, SessionCreated, SessionRenamed,
    ShepherdFrame, WorkspaceSelected, WorktreeCreated, WorktreeHook,
};
