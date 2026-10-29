//! The `Pty` contract. Commands used here (`printf`, `cat`, `sh`, `printenv`, `pwd`) run for real on the
//! adapter; the fake gets scripted versions of them (see `fakes::pty`).
//!
//! Run it with [`pty_contract!`](crate::pty_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Pty` contract case. `$make` builds the port under test from an
/// `Arc<dyn PtyEventSink>`, e.g. `shepherd_testkit::pty_contract!(PortablePty::new);`.
#[macro_export]
macro_rules! pty_contract {
    ($make:expr) => {
        $crate::pty_contract!(@cases $make;
            output_then_exit_code,
            exit_is_sent_once_and_last,
            short_lived_sessions_deliver_all_output,
            writes_reach_the_child,
            resize_is_visible_to_the_child,
            kill_ends_the_process,
            kill_ends_a_process_that_ignores_hangup,
            env_is_honored,
            cwd_is_honored,
            exited_session_is_unknown,
            unknown_session_is_an_error,
            spawning_a_live_id_again_is_already_exists,
            spawning_a_missing_command_is_spawn_failed,
        );
    };
    (@cases $make:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::pty::$case($crate::contracts::pty::PtyHarness::new($make));
            }
        )*
    };
}

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};

const TIMEOUT: Duration = Duration::from_secs(10);
/// How long to watch for stray events after an exit. Only used to assert that nothing else arrives.
const QUIET: Duration = Duration::from_millis(300);

/// A port plus the receiving end of the sink it was built with.
pub struct PtyHarness<P> {
    pub pty: P,
    events: Receiver<PtyEvent>,
}

impl<P: Pty> PtyHarness<P> {
    pub fn new(make: impl FnOnce(Arc<dyn PtyEventSink>) -> P) -> Self {
        let (tx, events) = mpsc::channel();
        let pty = make(Arc::new(ChannelSink(tx)));
        Self { pty, events }
    }

    /// Returns every event received up to and including the first one matching `predicate`.
    /// Panics if none matches within `timeout`.
    pub fn wait_for(
        &self,
        mut predicate: impl FnMut(&PtyEvent) -> bool,
        timeout: Duration,
    ) -> Vec<PtyEvent> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(event) => {
                    let done = predicate(&event);
                    seen.push(event);
                    if done {
                        return seen;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no matching pty event within {timeout:?}; saw {seen:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("pty event sink dropped; saw {seen:?}")
                }
            }
        }
    }

    /// Waits for `session` to exit. Returns its output (lossy UTF-8) and exit code.
    pub fn run_to_exit(&self, session: &SessionId) -> (String, Option<i32>) {
        let events = self.wait_for(|e| is_exit_of(e, session), TIMEOUT);
        let mut output = Vec::new();
        let mut code = None;
        for event in events {
            match event {
                PtyEvent::Output { session: s, bytes } if &s == session => output.extend(bytes),
                PtyEvent::Exited { code: c, .. } => code = c,
                PtyEvent::Output { .. } => {}
            }
        }
        (String::from_utf8_lossy(&output).into_owned(), code)
    }

    /// Waits until `session`'s accumulated output contains `needle`.
    pub fn wait_for_output(&self, session: &SessionId, needle: &str) {
        let mut output = Vec::new();
        self.wait_for(
            |e| match e {
                PtyEvent::Output { session: s, bytes } if s == session => {
                    output.extend_from_slice(bytes);
                    String::from_utf8_lossy(&output).contains(needle)
                }
                _ => false,
            },
            TIMEOUT,
        );
    }

    /// Panics if any event arrives within `window`.
    pub fn assert_quiet(&self, window: Duration) {
        if let Ok(event) = self.events.recv_timeout(window) {
            panic!("expected no more pty events, got {event:?}");
        }
    }
}

/// A sink that forwards events to an mpsc channel. Send errors (receiver gone) are ignored.
pub struct ChannelSink(pub Sender<PtyEvent>);

impl PtyEventSink for ChannelSink {
    fn send(&self, event: PtyEvent) {
        let _ = self.0.send(event);
    }
}

/// A spec for `command args..` in an 80x24 PTY, in the system temp dir, with no extra env.
pub fn spec(command: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: temp_dir(),
        env: Vec::new(),
        size: PtySize { cols: 80, rows: 24 },
    }
}

/// The system temp dir with symlinks resolved, so `pwd` prints it verbatim (macOS `/var` -> `/private/var`).
fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir();
    dir.canonicalize().unwrap_or(dir)
}

fn is_exit_of(event: &PtyEvent, session: &SessionId) -> bool {
    matches!(event, PtyEvent::Exited { session: s, .. } if s == session)
}

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

pub fn output_then_exit_code<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output, "hi");
    assert_eq!(code, Some(0));
}

pub fn exit_is_sent_once_and_last<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    h.assert_quiet(QUIET);
}

/// Children that exit right away must not lose output, even when many run at once.
pub fn short_lived_sessions_deliver_all_output<P: Pty>(mut h: PtyHarness<P>) {
    const N: usize = 20;
    let expected = |i: usize| format!("output-of-session-{i}");
    for i in 0..N {
        h.pty
            .spawn(sid(&format!("s{i}")), spec("printf", &[&expected(i)]))
            .unwrap();
    }

    let mut exits = 0;
    let events = h.wait_for(
        |e| {
            exits += usize::from(matches!(e, PtyEvent::Exited { .. }));
            exits == N
        },
        TIMEOUT,
    );

    for i in 0..N {
        let s = sid(&format!("s{i}"));
        let mut output = Vec::new();
        let mut exited = false;
        for event in &events {
            match event {
                PtyEvent::Output { session, bytes } if *session == s => {
                    assert!(!exited, "{s}: output after Exited");
                    output.extend_from_slice(bytes);
                }
                PtyEvent::Exited { session, code } if *session == s => {
                    assert!(!exited, "{s}: second Exited");
                    assert_eq!(*code, Some(0), "{s}");
                    exited = true;
                }
                _ => {}
            }
        }
        assert_eq!(String::from_utf8_lossy(&output), expected(i), "{s}");
    }
}

pub fn writes_reach_the_child<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    // With echo off, anything we see after "ready" was printed by cat, not echoed by the terminal.
    h.pty
        .spawn(
            s.clone(),
            spec("sh", &["-c", "stty -echo; echo ready; cat"]),
        )
        .unwrap();
    h.wait_for_output(&s, "ready");
    h.pty.write(&s, b"hello\n").unwrap();
    h.wait_for_output(&s, "hello");
    // ^D at the start of a line is EOF to cat.
    h.pty.write(&s, b"\x04").unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, Some(0));
}

pub fn resize_is_visible_to_the_child<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("stty");
    h.pty
        .spawn(s.clone(), spec("sh", &["-c", "read _; stty size"]))
        .unwrap();
    h.pty
        .resize(
            &s,
            PtySize {
                cols: 100,
                rows: 50,
            },
        )
        .unwrap();
    h.pty.write(&s, b"\n").unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert!(output.contains("50 100"), "stty size printed {output:?}");
    assert_eq!(code, Some(0));
}

pub fn kill_ends_the_process<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    h.pty.spawn(s.clone(), spec("cat", &[])).unwrap();
    h.pty.kill(&s).unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, None, "a killed process has no exit code");
}

pub fn kill_ends_a_process_that_ignores_hangup<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("stubborn");
    h.pty
        .spawn(
            s.clone(),
            spec("sh", &["-c", "trap '' HUP; echo ready; cat"]),
        )
        .unwrap();
    h.wait_for_output(&s, "ready");
    h.pty.kill(&s).unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, None, "a killed process has no exit code");
}

pub fn env_is_honored<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printenv");
    let mut spec = spec("printenv", &["SHEPHERD_PTY_CONTRACT"]);
    spec.env
        .push(("SHEPHERD_PTY_CONTRACT".into(), "from-spec".into()));
    h.pty.spawn(s.clone(), spec).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output.trim_end(), "from-spec");
    assert_eq!(code, Some(0));
}

pub fn cwd_is_honored<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("pwd");
    let spec = spec("pwd", &[]);
    let cwd = spec.cwd.display().to_string();
    h.pty.spawn(s.clone(), spec).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output.trim_end(), cwd);
    assert_eq!(code, Some(0));
}

pub fn exited_session_is_unknown<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    assert_eq!(h.pty.write(&s, b"x"), Err(PtyError::UnknownSession));
    // The id is free again.
    h.pty.spawn(s.clone(), spec("printf", &["again"])).unwrap();
    assert_eq!(h.run_to_exit(&s), ("again".into(), Some(0)));
}

pub fn unknown_session_is_an_error<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("nobody");
    let size = PtySize { cols: 80, rows: 24 };
    assert_eq!(h.pty.write(&s, b"x"), Err(PtyError::UnknownSession));
    assert_eq!(h.pty.resize(&s, size), Err(PtyError::UnknownSession));
    assert_eq!(h.pty.kill(&s), Err(PtyError::UnknownSession));
}

pub fn spawning_a_live_id_again_is_already_exists<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    h.pty.spawn(s.clone(), spec("cat", &[])).unwrap();
    assert_eq!(
        h.pty.spawn(s.clone(), spec("cat", &[])),
        Err(PtyError::AlreadyExists)
    );
    h.pty.kill(&s).unwrap();
    h.run_to_exit(&s);
}

pub fn spawning_a_missing_command_is_spawn_failed<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("missing");
    let result = h
        .pty
        .spawn(s.clone(), spec("shepherd-no-such-command", &[]));
    assert!(
        matches!(result, Err(PtyError::SpawnFailed(_))),
        "got {result:?}"
    );
    assert_eq!(h.pty.write(&s, b"x"), Err(PtyError::UnknownSession));
}
