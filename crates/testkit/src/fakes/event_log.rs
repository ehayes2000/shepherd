//! An in-memory `EventLog`. It has no storage, so "reopening" it means keeping the same value.
//!
//! Tests can preload entries (`with_envelopes`, e.g. from an `EnvelopeBuilder`) and inject the next failure
//! (`fail_next`).

use std::cell::Cell;
use std::collections::BTreeMap;

use shepherd_core::acpp::{Envelope, FrameDraft, Seq, Stamp};
use shepherd_core::ports::{EventLog, LogError, LogId};

#[derive(Default)]
pub struct FakeEventLog {
    logs: BTreeMap<LogId, Vec<Envelope>>,
    fail_next: Cell<Option<LogError>>,
}

impl FakeEventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends already-sealed envelopes to `log`, as if they had been written before. Panics unless their seqs
    /// continue the log without gaps and their session matches `log`, since a real log can't hold anything else.
    pub fn with_envelopes(
        mut self,
        log: LogId,
        envelopes: impl IntoIterator<Item = Envelope>,
    ) -> Self {
        let entries = self.logs.entry(log.clone()).or_default();
        for env in envelopes {
            let expected = entries.len() as u64 + 1;
            assert_eq!(
                env.seq().get(),
                expected,
                "FakeEventLog: preloaded {log} entry has the wrong seq"
            );
            assert_eq!(
                env.session(),
                log.session(),
                "FakeEventLog: preloaded {log} entry is for another session"
            );
            entries.push(env);
        }
        if entries.is_empty() {
            self.logs.remove(&log);
        }
        self
    }

    /// Makes the next call (of any method) fail with `error`, without touching the log.
    pub fn fail_next(&self, error: LogError) {
        self.fail_next.set(Some(error));
    }

    fn injected(&self) -> Result<(), LogError> {
        self.fail_next.take().map_or(Ok(()), Err)
    }
}

impl EventLog for FakeEventLog {
    fn append(
        &mut self,
        log: LogId,
        draft: FrameDraft,
        stamp: Stamp,
    ) -> Result<Envelope, LogError> {
        self.injected()?;
        if LogId::of(&draft) != log {
            return Err(LogError::WrongLog {
                log,
                draft: draft.session,
            });
        }
        log.validate()?;
        let entries = self.logs.entry(log).or_default();
        let env = draft.seal(Seq::new(entries.len() as u64 + 1), stamp);
        entries.push(env.clone());
        Ok(env)
    }

    fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, LogError> {
        self.injected()?;
        log.validate()?;
        let entries = self.logs.get(&log).map_or(&[][..], Vec::as_slice);
        Ok(entries.iter().filter(|e| e.seq() > from).cloned().collect())
    }

    fn logs(&self) -> Result<Vec<LogId>, LogError> {
        self.injected()?;
        Ok(self.logs.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::builders::EnvelopeBuilder;
    use crate::contracts::event_log::{EventLogHarness, session};
    use shepherd_core::ids::SessionId;

    fn harness() -> EventLogHarness<FakeEventLog> {
        EventLogHarness::new(|old| old.unwrap_or_default())
    }

    crate::event_log_contract!(harness);

    #[test]
    fn preloaded_entries_are_read_back_and_appends_continue_after_them() {
        let mut builder = EnvelopeBuilder::new().session(SessionId::new("s1"));
        let preloaded = vec![
            builder.internal(json!({ "jsonrpc": "2.0", "method": "a" })),
            builder.internal(json!({ "jsonrpc": "2.0", "method": "b" })),
        ];
        let s = session("s1");
        let mut log = FakeEventLog::new().with_envelopes(s.clone(), preloaded.clone());

        assert_eq!(log.read(s.clone(), Seq::ZERO).unwrap(), preloaded);
        let draft = crate::contracts::event_log::draft(&s, json!({}));
        let stamp = crate::contracts::event_log::stamp(3);
        assert_eq!(log.append(s, draft, stamp).unwrap().seq(), Seq::new(3));
    }

    #[test]
    #[should_panic(expected = "wrong seq")]
    fn preloading_a_gap_panics() {
        let mut builder = EnvelopeBuilder::new().session(SessionId::new("s1"));
        builder.internal(json!({ "jsonrpc": "2.0", "method": "skipped" }));
        let second = builder.internal(json!({ "jsonrpc": "2.0", "method": "b" }));
        let _ = FakeEventLog::new().with_envelopes(session("s1"), [second]);
    }

    #[test]
    fn injected_failure_hits_the_next_call_only() {
        let mut log = FakeEventLog::new();
        log.fail_next(LogError::Io("disk full".into()));
        let draft = crate::contracts::event_log::draft(&LogId::Host, json!({}));
        let stamp = crate::contracts::event_log::stamp(1);
        assert_eq!(
            log.append(LogId::Host, draft.clone(), stamp.clone()),
            Err(LogError::Io("disk full".into()))
        );
        assert_eq!(
            log.append(LogId::Host, draft, stamp).unwrap().seq(),
            Seq::new(1)
        );
    }
}
