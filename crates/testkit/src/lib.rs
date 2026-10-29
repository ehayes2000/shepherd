//! Test support shared by every crate: fakes, contract suites, and builders.
//!
//! Depend on this crate only under `[dev-dependencies]`.
//!
//! `shepherd-core` may use it only from integration tests in `crates/core/tests/`, never from `#[cfg(test)]` unit
//! tests: a core unit test would link two copies of `shepherd-core`, and their types would not match.

/// Builders for domain values (`AppBuilder`, `SessionBuilder`, ...), so tests don't construct large structs inline.
pub mod builders {
    use serde_json::Value;
    use shepherd_core::acpp::{
        Dir, EntryId, Envelope, Frame, FrameDraft, Seq, Source, SourceKind, Stamp, Timestamp,
    };
    use shepherd_core::ids::{MachineId, SessionId};

    /// Seals frames into envelopes for one session log, as the host's append path would: `seq` starts at 1 and
    /// increments with every envelope built.
    ///
    /// ```text
    /// let mut log = EnvelopeBuilder::new();
    /// let env = log.to_agent(json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": { .. } }));
    /// ```
    #[derive(Debug, Clone)]
    pub struct EnvelopeBuilder {
        next_seq: u64,
        session: SessionId,
        source: Source,
    }

    impl Default for EnvelopeBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EnvelopeBuilder {
        /// A log for session `s_test`, with frames from a `hook` source for `claude-code`.
        pub fn new() -> Self {
            Self {
                next_seq: 1,
                session: SessionId::new("s_test"),
                source: Source::new(SourceKind::Hook, Some("claude-code".to_owned())),
            }
        }

        pub fn session(mut self, session: SessionId) -> Self {
            self.session = session;
            self
        }

        pub fn source(mut self, source: Source) -> Self {
            self.source = source;
            self
        }

        /// The seq the next envelope will get.
        pub fn next_seq(&self) -> Seq {
            Seq::new(self.next_seq)
        }

        /// Seals `frame` (JSON-RPC 2.0 JSON) with direction `dir` at the next seq. Panics on a malformed frame.
        pub fn envelope(&mut self, dir: Dir, frame: Value) -> Envelope {
            let frame = Frame::from_value(frame)
                .unwrap_or_else(|e| panic!("EnvelopeBuilder: malformed frame: {e}"));
            let seq = self.next_seq;
            self.next_seq += 1;
            let ts = Timestamp::parse("2026-10-29T12:00:00.000Z")
                .unwrap_or_else(|e| panic!("EnvelopeBuilder: bad timestamp: {e}"));
            let stamp = Stamp {
                id: EntryId::new(format!("e{seq}")),
                ts,
                machine: MachineId::new("m_test"),
            };
            FrameDraft::new(frame, self.source.clone(), dir, Some(self.session.clone()))
                .seal(Seq::new(seq), stamp)
        }

        pub fn to_agent(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::ToAgent, frame)
        }

        pub fn from_agent(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::FromAgent, frame)
        }

        pub fn internal(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::Internal, frame)
        }
    }
}

/// One shared suite per port: `pub fn suite<P: Port>(make: impl Fn(..) -> P)`, run against the fake and the real
/// adapter. `make` takes whatever the port is constructed with (e.g. an event sink).
pub mod contracts {
    pub mod config;
    pub mod pty;
}

/// One stateful in-memory fake per port. Each must pass its port's contract suite.
pub mod fakes {
    pub mod config;
    pub mod pty;
}
