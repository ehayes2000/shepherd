//! The `PtyHandoff` contract: sessions exported by one instance and adopted by a fresh one keep working, and an export
//! that is called off changes nothing. Commands are the ones the `Pty` contract uses, plus `sh -c "read _; exit 7"`.
//!
//! Run it with [`pty_handoff_contract!`](crate::pty_handoff_contract), which expands to one `#[test]` per case. Add
//! every new case to the macro's list.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `PtyHandoff` contract case. `$make` builds a fresh instance of the port under test
/// from an `Arc<dyn PtyEventSink>`; each case builds two, the exporter and the adopter.
#[macro_export]
macro_rules! pty_handoff_contract {
    ($make:expr) => {
        $crate::pty_handoff_contract!(@cases $make;
            adopted_session_echoes_writes,
            adopted_session_sees_resizes,
            adopted_session_can_be_killed,
            adopted_session_reports_its_exit_code,
            exported_sessions_are_unknown_to_the_exporter,
            export_then_resume_changes_nothing,
            export_leaves_out_sessions_that_exited,
            adopting_a_live_id_again_is_already_exists,
        );
    };
    (@cases $make:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::pty_handoff::$case($make);
            }
        )*
    };
}

use std::sync::Arc;

use shepherd_core::ids::SessionId;
use shepherd_core::ports::{PtyError, PtyEventSink, PtyHandoff, PtySize};

use super::pty::{PtyHarness, QUIET, spec};

/// With echo off, anything after "ready" was printed by cat, not echoed by the terminal.
const CAT: &str = "stty -echo; echo ready; cat";

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

/// Spawns `script` as `s` in a new exporter, waits for `ready` if it prints it, exports, and adopts every token into
/// a new adopter. Returns both.
fn hand_over<P: PtyHandoff>(
    make: &impl Fn(Arc<dyn PtyEventSink>) -> P,
    s: &SessionId,
    script: &str,
    ready: bool,
) -> (PtyHarness<P>, PtyHarness<P>) {
    let mut old = PtyHarness::new(make);
    old.pty
        .spawn(s.clone(), spec("sh", &["-c", script]))
        .unwrap();
    if ready {
        old.wait_for_output(s, "ready");
    }
    let tokens = old.pty.export().unwrap();
    let ids: Vec<&SessionId> = tokens.iter().map(|(id, _)| id).collect();
    assert_eq!(ids, vec![s]);
    let mut new = PtyHarness::new(make);
    for (id, token) in tokens {
        new.pty.adopt(id, token).unwrap();
    }
    (old, new)
}

pub fn adopted_session_echoes_writes<P: PtyHandoff>(make: impl Fn(Arc<dyn PtyEventSink>) -> P) {
    let s = sid("cat");
    let (_old, mut new) = hand_over(&make, &s, CAT, true);
    new.pty.write(&s, b"after-handoff\n").unwrap();
    new.wait_for_output(&s, "after-handoff");
}

pub fn adopted_session_sees_resizes<P: PtyHandoff>(make: impl Fn(Arc<dyn PtyEventSink>) -> P) {
    let s = sid("size");
    let (_old, mut new) = hand_over(&make, &s, "read _; stty size", false);
    new.pty
        .resize(
            &s,
            PtySize {
                cols: 101,
                rows: 33,
            },
        )
        .unwrap();
    new.pty.write(&s, b"\n").unwrap();
    let (output, code) = new.run_to_exit(&s);
    assert!(output.contains("33 101"), "{output:?}");
    assert_eq!(code, Some(0));
}

pub fn adopted_session_can_be_killed<P: PtyHandoff>(make: impl Fn(Arc<dyn PtyEventSink>) -> P) {
    let s = sid("stubborn");
    let (_old, mut new) = hand_over(&make, &s, "trap '' HUP; echo ready; cat", true);
    new.pty.kill(&s).unwrap();
    let (_, code) = new.run_to_exit(&s);
    assert_eq!(code, None);
    new.assert_quiet(QUIET);
}

pub fn adopted_session_reports_its_exit_code<P: PtyHandoff>(
    make: impl Fn(Arc<dyn PtyEventSink>) -> P,
) {
    let s = sid("seven");
    let (old, mut new) = hand_over(&make, &s, "read _; exit 7", false);
    new.pty.write(&s, b"\n").unwrap();
    let (_, code) = new.run_to_exit(&s);
    assert_eq!(code, Some(7));
    // The exit is the adopter's to report, once.
    old.assert_quiet(QUIET);
    new.assert_quiet(QUIET);
}

pub fn exported_sessions_are_unknown_to_the_exporter<P: PtyHandoff>(
    make: impl Fn(Arc<dyn PtyEventSink>) -> P,
) {
    let s = sid("cat");
    let mut old = PtyHarness::new(&make);
    old.pty.spawn(s.clone(), spec("sh", &["-c", CAT])).unwrap();
    old.wait_for_output(&s, "ready");
    let _tokens = old.pty.export().unwrap();

    assert_eq!(old.pty.write(&s, b"x\n"), Err(PtyError::UnknownSession));
    assert_eq!(
        old.pty.resize(&s, PtySize { cols: 10, rows: 10 }),
        Err(PtyError::UnknownSession)
    );
    assert_eq!(old.pty.kill(&s), Err(PtyError::UnknownSession));
    old.assert_quiet(QUIET);
    old.pty.resume();
    old.pty.kill(&s).unwrap();
    old.run_to_exit(&s);
}

pub fn export_then_resume_changes_nothing<P: PtyHandoff>(
    make: impl Fn(Arc<dyn PtyEventSink>) -> P,
) {
    let s = sid("cat");
    let mut h = PtyHarness::new(&make);
    h.pty.spawn(s.clone(), spec("sh", &["-c", CAT])).unwrap();
    h.wait_for_output(&s, "ready");

    let tokens = h.pty.export().unwrap();
    assert_eq!(tokens.len(), 1);
    drop(tokens);
    h.pty.resume();

    h.pty.write(&s, b"still-here\n").unwrap();
    h.wait_for_output(&s, "still-here");
    h.pty.resize(&s, PtySize { cols: 90, rows: 30 }).unwrap();
    h.pty.write(&s, b"\x04").unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, Some(0));
    h.assert_quiet(QUIET);
}

pub fn export_leaves_out_sessions_that_exited<P: PtyHandoff>(
    make: impl Fn(Arc<dyn PtyEventSink>) -> P,
) {
    let s = sid("printf");
    let mut h = PtyHarness::new(&make);
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    let tokens = h.pty.export().unwrap();
    assert!(tokens.is_empty(), "{tokens:?}");
    h.pty.resume();
}

pub fn adopting_a_live_id_again_is_already_exists<P: PtyHandoff>(
    make: impl Fn(Arc<dyn PtyEventSink>) -> P,
) {
    let s = sid("cat");
    let (_old, mut new) = hand_over(&make, &s, CAT, true);
    // Another session under the same id, exported from a second exporter.
    let mut other = PtyHarness::new(&make);
    other
        .pty
        .spawn(s.clone(), spec("sh", &["-c", CAT]))
        .unwrap();
    other.wait_for_output(&s, "ready");
    let mut tokens = other.pty.export().unwrap();
    let (_, token) = tokens.remove(0);

    assert_eq!(
        new.pty.adopt(s.clone(), token),
        Err(PtyError::AlreadyExists)
    );
    // The adopted one is untouched. (That the refused one is killed is for each adapter to show: see `pty.rs`.)
    new.pty.write(&s, b"first\n").unwrap();
    new.wait_for_output(&s, "first");
}
