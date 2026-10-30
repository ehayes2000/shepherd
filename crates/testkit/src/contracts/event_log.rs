//! The `EventLog` contract. Storage-specific failures (torn writes, corrupt files) are tested next to the
//! adapter that has storage; this suite is what every implementation must agree on.
//!
//! Run it with [`event_log_contract!`](crate::event_log_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `EventLog` contract case. `$make` is a function returning a fresh, empty
/// [`EventLogHarness`](crate::contracts::event_log::EventLogHarness), e.g.
/// `shepherd_testkit::event_log_contract!(temp_harness);`.
///
/// New cases go in the list below, or they never run.
#[macro_export]
macro_rules! event_log_contract {
    ($make:expr) => {
        $crate::event_log_contract!(@cases $make;
            first_append_gets_seq_one,
            seq_is_gapless,
            append_returns_the_sealed_draft,
            read_from_zero_returns_every_entry_in_order,
            read_returns_entries_after_from,
            read_past_the_end_is_empty,
            reading_a_log_with_no_entries_is_empty,
            seq_is_per_log,
            logs_lists_logs_with_entries_sorted,
            logs_of_a_new_store_is_empty,
            reopen_returns_identical_entries,
            reopen_continues_seq,
            many_interleaved_appends_stay_gapless_across_reopen,
            awkward_frame_content_survives_reopen,
            append_to_the_wrong_log_is_wrong_log,
            unsafe_session_ids_are_invalid,
            read_batch_returns_at_most_max_entries_after_from,
            consecutive_batches_cover_the_log_exactly_once,
            read_batch_at_the_end_or_of_an_empty_log_is_empty,
            read_batch_sees_entries_appended_after_a_reopen,
        );
    };
    (@cases $make:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::event_log::$case($make());
            }
        )*
    };
}

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};
use shepherd_core::acpp::{
    Dir, EntryId, Envelope, Frame, FrameDraft, Seq, Source, SourceKind, Stamp, Timestamp,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{EventLog, LogError, LogId};

type Open<L> = Box<dyn FnMut(Option<L>) -> L>;

/// A log under test, plus how to reopen it on the same storage. Derefs to the log.
pub struct EventLogHarness<L> {
    log: Option<L>,
    open: Open<L>,
}

impl<L: EventLog> EventLogHarness<L> {
    /// `open(None)` opens the store for the first time. `open(Some(old))` reopens it: an adapter drops `old` and
    /// opens its storage again; a fake with no storage may hand `old` back.
    pub fn new(mut open: impl FnMut(Option<L>) -> L + 'static) -> Self {
        let log = open(None);
        Self {
            log: Some(log),
            open: Box::new(open),
        }
    }

    /// Closes the log and opens it again on the same storage.
    pub fn reopen(&mut self) {
        let old = self.log.take();
        self.log = Some((self.open)(old));
    }

    /// Every entry of `log`.
    pub fn all(&self, log: &LogId) -> Vec<Envelope> {
        self.read(log.clone(), Seq::ZERO).unwrap()
    }

    /// Appends a test frame tagged `n` to `log`, returning the envelope.
    pub fn push(&mut self, log: &LogId, n: u64) -> Envelope {
        let draft = draft(log, json!({ "n": n }));
        self.append(log.clone(), draft, stamp(n)).unwrap()
    }
}

impl<L> Deref for EventLogHarness<L> {
    type Target = L;

    fn deref(&self) -> &L {
        self.log.as_ref().unwrap()
    }
}

impl<L> DerefMut for EventLogHarness<L> {
    fn deref_mut(&mut self) -> &mut L {
        self.log.as_mut().unwrap()
    }
}

/// A draft for `log` carrying a `_test/entry` notification with `params`.
pub fn draft(log: &LogId, params: Value) -> FrameDraft {
    let frame =
        Frame::from_value(json!({ "jsonrpc": "2.0", "method": "_test/entry", "params": params }))
            .unwrap();
    FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        log.session().cloned(),
    )
}

/// A stamp whose id is `e<n>`.
pub fn stamp(n: u64) -> Stamp {
    Stamp {
        id: EntryId::new(format!("e{n}")),
        ts: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap(),
        machine: MachineId::new("m_test"),
    }
}

pub fn session(id: &str) -> LogId {
    LogId::Session(SessionId::new(id))
}

fn seqs(entries: &[Envelope]) -> Vec<u64> {
    entries.iter().map(|e| e.seq().get()).collect()
}

pub fn first_append_gets_seq_one<L: EventLog>(mut h: EventLogHarness<L>) {
    assert_eq!(h.push(&LogId::Host, 1).seq(), Seq::new(1));
}

pub fn seq_is_gapless<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let got: Vec<u64> = (1..=5).map(|n| h.push(&s, n).seq().get()).collect();
    assert_eq!(got, vec![1, 2, 3, 4, 5]);
}

pub fn append_returns_the_sealed_draft<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    h.push(&s, 1);
    let d = draft(&s, json!({ "hello": "world" }));
    let got = h.append(s.clone(), d.clone(), stamp(7)).unwrap();
    assert_eq!(got, d.seal(Seq::new(2), stamp(7)));
}

pub fn read_from_zero_returns_every_entry_in_order<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let appended: Vec<Envelope> = (1..=4).map(|n| h.push(&s, n)).collect();
    assert_eq!(h.all(&s), appended);
}

pub fn read_returns_entries_after_from<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let appended: Vec<Envelope> = (1..=5).map(|n| h.push(&s, n)).collect();
    assert_eq!(h.read(s.clone(), Seq::new(2)).unwrap(), appended[2..]);
    assert_eq!(h.read(s.clone(), Seq::new(4)).unwrap(), appended[4..]);
}

pub fn read_past_the_end_is_empty<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    h.push(&s, 1);
    h.push(&s, 2);
    assert_eq!(h.read(s.clone(), Seq::new(2)).unwrap(), vec![]);
    assert_eq!(h.read(s.clone(), Seq::new(100)).unwrap(), vec![]);
}

pub fn read_batch_returns_at_most_max_entries_after_from<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let appended: Vec<Envelope> = (1..=10).map(|n| h.push(&s, n)).collect();
    assert_eq!(
        h.read_batch(s.clone(), Seq::ZERO, 3).unwrap(),
        appended[..3]
    );
    assert_eq!(
        h.read_batch(s.clone(), Seq::new(4), 3).unwrap(),
        appended[4..7]
    );
    // Fewer than `max` left: just those.
    assert_eq!(
        h.read_batch(s.clone(), Seq::new(8), 5).unwrap(),
        appended[8..]
    );
    assert_eq!(h.read_batch(s.clone(), Seq::ZERO, 0).unwrap(), vec![]);
}

pub fn consecutive_batches_cover_the_log_exactly_once<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    for n in 1..=23 {
        h.push(&s, n);
        h.push(&LogId::Host, n);
    }
    let mut got = Vec::new();
    let mut from = Seq::ZERO;
    loop {
        let batch = h.read_batch(s.clone(), from, 5).unwrap();
        let Some(last) = batch.last() else {
            break;
        };
        assert!(batch.len() <= 5);
        from = last.seq();
        got.extend(batch);
    }
    assert_eq!(got, h.all(&s));
}

pub fn read_batch_at_the_end_or_of_an_empty_log_is_empty<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    h.push(&s, 1);
    h.push(&s, 2);
    assert_eq!(h.read_batch(s.clone(), Seq::new(2), 10).unwrap(), vec![]);
    assert_eq!(h.read_batch(s.clone(), Seq::new(100), 10).unwrap(), vec![]);
    assert_eq!(h.read_batch(LogId::Host, Seq::ZERO, 10).unwrap(), vec![]);
    assert_eq!(
        h.read_batch(session("nobody"), Seq::ZERO, 10).unwrap(),
        vec![]
    );
}

pub fn read_batch_sees_entries_appended_after_a_reopen<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let mut appended: Vec<Envelope> = (1..=4).map(|n| h.push(&s, n)).collect();
    h.reopen();
    appended.extend((5..=6).map(|n| h.push(&s, n)));
    assert_eq!(
        h.read_batch(s.clone(), Seq::new(2), 3).unwrap(),
        appended[2..5]
    );
    assert_eq!(
        h.read_batch(s.clone(), Seq::new(5), 3).unwrap(),
        appended[5..]
    );
}

pub fn reading_a_log_with_no_entries_is_empty<L: EventLog>(h: EventLogHarness<L>) {
    assert_eq!(h.read(LogId::Host, Seq::ZERO).unwrap(), vec![]);
    assert_eq!(h.read(session("nobody"), Seq::ZERO).unwrap(), vec![]);
}

pub fn seq_is_per_log<L: EventLog>(mut h: EventLogHarness<L>) {
    let (a, b) = (session("a"), session("b"));
    let got = [
        h.push(&a, 1).seq().get(),
        h.push(&LogId::Host, 2).seq().get(),
        h.push(&a, 3).seq().get(),
        h.push(&b, 4).seq().get(),
        h.push(&a, 5).seq().get(),
        h.push(&LogId::Host, 6).seq().get(),
    ];
    assert_eq!(got, [1, 1, 2, 1, 3, 2]);
    assert_eq!(seqs(&h.all(&a)), [1, 2, 3]);
    assert_eq!(seqs(&h.all(&b)), [1]);
    assert_eq!(seqs(&h.all(&LogId::Host)), [1, 2]);
}

pub fn logs_lists_logs_with_entries_sorted<L: EventLog>(mut h: EventLogHarness<L>) {
    h.push(&session("zeta"), 1);
    h.push(&session("alpha"), 2);
    h.push(&LogId::Host, 3);
    assert_eq!(
        h.logs().unwrap(),
        vec![LogId::Host, session("alpha"), session("zeta")]
    );
}

pub fn logs_of_a_new_store_is_empty<L: EventLog>(h: EventLogHarness<L>) {
    assert_eq!(h.logs().unwrap(), vec![]);
    // Reading a log doesn't create it.
    h.read(session("s1"), Seq::ZERO).unwrap();
    assert_eq!(h.logs().unwrap(), vec![]);
}

pub fn reopen_returns_identical_entries<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let host: Vec<Envelope> = (1..=3).map(|n| h.push(&LogId::Host, n)).collect();
    let sess: Vec<Envelope> = (1..=3).map(|n| h.push(&s, n)).collect();
    h.reopen();
    assert_eq!(h.all(&LogId::Host), host);
    assert_eq!(h.all(&s), sess);
    assert_eq!(h.logs().unwrap(), vec![LogId::Host, s]);
}

pub fn reopen_continues_seq<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    h.push(&s, 1);
    h.push(&s, 2);
    h.reopen();
    assert_eq!(h.push(&s, 3).seq(), Seq::new(3));
    assert_eq!(h.push(&LogId::Host, 4).seq(), Seq::new(1));
    assert_eq!(seqs(&h.all(&s)), [1, 2, 3]);
}

pub fn many_interleaved_appends_stay_gapless_across_reopen<L: EventLog>(mut h: EventLogHarness<L>) {
    const N: u64 = 2000;
    let logs = [LogId::Host, session("a"), session("b"), session("c")];
    for n in 0..N {
        let log = &logs[(n * 7 % 4) as usize];
        h.push(log, n);
        if n == N / 2 {
            h.reopen();
        }
    }
    h.reopen();
    for log in &logs {
        let entries = h.all(log);
        let expected: Vec<u64> = (1..=entries.len() as u64).collect();
        assert_eq!(seqs(&entries), expected, "{log}");
        assert_eq!(entries.len() as u64, N / 4, "{log}");
    }
}

/// Newlines, unicode, escapes and big payloads inside a frame must not break line-oriented storage.
pub fn awkward_frame_content_survives_reopen<L: EventLog>(mut h: EventLogHarness<L>) {
    let s = session("s1");
    let params = json!({
        "text": "line one\nline two\r\n\ttabbed \"quoted\" \\ back\u{0}slash",
        "unicode": "héllo 🐑 日本語 \u{2028}\u{2029}",
        "big": "x".repeat(256 * 1024),
        "nested": { "array": [1, 2.5, null, true, { "deep": [] }] },
    });
    let d = draft(&s, params);
    let appended = h.append(s.clone(), d, stamp(1)).unwrap();
    let after = h.push(&s, 2);
    h.reopen();
    assert_eq!(h.all(&s), vec![appended, after]);
}

pub fn append_to_the_wrong_log_is_wrong_log<L: EventLog>(mut h: EventLogHarness<L>) {
    let (a, b) = (session("a"), session("b"));
    let cases = [
        (LogId::Host, draft(&a, json!({}))),
        (a.clone(), draft(&LogId::Host, json!({}))),
        (a.clone(), draft(&b, json!({}))),
    ];
    for (log, d) in cases {
        let draft_session = d.session.clone();
        assert_eq!(
            h.append(log.clone(), d, stamp(1)),
            Err(LogError::WrongLog {
                log,
                draft: draft_session
            })
        );
    }
    // Nothing was stored and no seq was used.
    assert_eq!(h.logs().unwrap(), vec![]);
    assert_eq!(h.push(&a, 1).seq(), Seq::new(1));
}

pub fn unsafe_session_ids_are_invalid<L: EventLog>(mut h: EventLogHarness<L>) {
    let too_long = "a".repeat(65);
    for bad in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "/abs",
        "a\\b",
        ".hidden",
        "nul\0",
        // Uppercase would alias lowercase on a case-insensitive filesystem.
        "S1",
        "a.b",
        "s 1",
        "é",
        &too_long,
    ] {
        let log = session(bad);
        let err = LogError::InvalidSessionId(SessionId::new(bad));
        assert_eq!(
            h.append(log.clone(), draft(&log, json!({})), stamp(1)),
            Err(err.clone()),
            "append {bad:?}"
        );
        assert_eq!(h.read(log.clone(), Seq::ZERO), Err(err), "read {bad:?}");
    }
    assert_eq!(h.logs().unwrap(), vec![]);
    // Lowercase letters, digits, `_` and `-`, up to 64 of them.
    let longest = "a".repeat(64);
    for ok in ["s_01hx-9z", "0", &longest] {
        assert_eq!(h.push(&session(ok), 1).seq(), Seq::new(1), "{ok:?}");
    }
}
