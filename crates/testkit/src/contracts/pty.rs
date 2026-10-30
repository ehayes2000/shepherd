//! The `Pty` contract. Commands used here (`printf`, `cat`, `sh`, `printenv`, `pwd`) run for real on the
//! adapter; the fake gets scripted versions of them (see `fakes::pty`).
//!
//! Run it with [`pty_contract!`](crate::pty_contract), which expands to one `#[test]` per case. Add every new case
//! to the macro's list.

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
            unacked_output_stops_reading_until_acked,
            kill_ends_a_session_blocked_on_unacked_output,
            ack_of_an_unknown_session_is_an_error,
            kill_drops_output_past_the_credit,
            acking_more_than_outstanding_banks_nothing,
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
use shepherd_core::ports::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec,
};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
/// How long to watch for stray events after an exit. Only used to assert that nothing else arrives.
pub(crate) const QUIET: Duration = Duration::from_millis(300);

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

    /// Collects `session`'s output, acknowledging none of it, until it holds at least `bytes`. Panics on its exit.
    pub fn output_unacked(&self, session: &SessionId, bytes: usize) -> Vec<u8> {
        let mut output = Vec::new();
        self.wait_for(
            |e| {
                output.extend(output_of(e, session));
                output.len() >= bytes
            },
            TIMEOUT,
        );
        output
    }

    /// Every output of `session` that arrives within `window`. Panics on any other event.
    pub fn output_within(&self, session: &SessionId, window: Duration) -> Vec<u8> {
        let deadline = Instant::now() + window;
        let mut output = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(PtyEvent::Output { session: s, bytes }) if &s == session => output.extend(bytes),
                Ok(event) => panic!("expected only output of {session}, got {event:?}"),
                Err(_) => return output,
            }
        }
    }

    /// Like [`run_to_exit`](Self::run_to_exit), but acknowledges each output as it arrives, and checks that no
    /// output is bigger than [`MAX_OUTPUT_CHUNK`].
    pub fn run_to_exit_acking(&mut self, session: &SessionId) -> (Vec<u8>, Option<i32>) {
        let deadline = Instant::now() + TIMEOUT;
        let mut output = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(PtyEvent::Output { session: s, bytes }) if &s == session => {
                    assert!(bytes.len() <= MAX_OUTPUT_CHUNK, "{} bytes", bytes.len());
                    // Racing the exit: the port may have forgotten the session already.
                    let _ = self.pty.ack(session, bytes.len());
                    output.extend(bytes);
                }
                Ok(PtyEvent::Exited { session: s, code }) if &s == session => {
                    return (output, code);
                }
                Ok(_) => {}
                Err(e) => panic!("{session} didn't exit: {e:?} after {} bytes", output.len()),
            }
        }
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

fn output_of<'a>(event: &'a PtyEvent, session: &SessionId) -> &'a [u8] {
    match event {
        PtyEvent::Output { session: s, bytes } if s == session => bytes,
        PtyEvent::Exited { session: s, .. } if s == session => {
            panic!("{session} exited early")
        }
        _ => &[],
    }
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

/// A child printing far more than [`OUTPUT_CREDIT`] while nothing is acknowledged stops being read: the bytes
/// delivered stop short of the credit plus one chunk, and the child can't finish. Acknowledged, it resumes and
/// delivers everything, in order, then exactly one `Exited`.
///
/// Output processing is off (`-opost`) because macOS's newline translation emits a stray `\r` when the pty's buffer
/// fills mid-`\r\n`, which is exactly what backpressure does. That is the kernel's doing, and harmless on a screen.
pub fn unacked_output_stops_reading_until_acked<P: Pty>(mut h: PtyHarness<P>) {
    let expected: String = (1..=400_000).map(|i| format!("{i}\n")).collect();
    assert!(expected.len() > 2 * OUTPUT_CREDIT);
    let s = sid("seq");
    h.pty
        .spawn(
            s.clone(),
            spec("sh", &["-c", "stty -opost; exec seq 1 400000"]),
        )
        .unwrap();

    let mut output = h.output_unacked(&s, OUTPUT_CREDIT);
    output.extend(h.output_within(&s, QUIET));
    assert!(
        output.len() < OUTPUT_CREDIT + MAX_OUTPUT_CHUNK,
        "{} bytes delivered with nothing acked",
        output.len()
    );

    h.pty.ack(&s, output.len()).unwrap();
    let (rest, code) = h.run_to_exit_acking(&s);
    output.extend(rest);
    if output != expected.as_bytes() {
        let at = output
            .iter()
            .zip(expected.as_bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(output.len().min(expected.len()));
        let around = |b: &[u8]| {
            String::from_utf8_lossy(&b[at.saturating_sub(40)..(at + 40).min(b.len())]).into_owned()
        };
        panic!(
            "output differs at byte {at} of {} (expected {}): got {:?}, expected {:?}",
            output.len(),
            expected.len(),
            around(&output),
            around(expected.as_bytes())
        );
    }
    assert_eq!(code, Some(0));
    h.assert_quiet(QUIET);
}

/// A child blocked on output nobody acknowledged can still be killed, and exits exactly once.
pub fn kill_ends_a_session_blocked_on_unacked_output<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("yes");
    h.pty.spawn(s.clone(), spec("yes", &[])).unwrap();
    h.output_unacked(&s, OUTPUT_CREDIT);
    h.pty.kill(&s).unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, None, "a killed process has no exit code");
    h.assert_quiet(QUIET);
}

pub fn ack_of_an_unknown_session_is_an_error<P: Pty>(mut h: PtyHarness<P>) {
    assert_eq!(h.pty.ack(&sid("nobody"), 1), Err(PtyError::UnknownSession));
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    assert_eq!(h.pty.ack(&s, 2), Err(PtyError::UnknownSession));
}

/// After a kill, output past the credit is dropped rather than waiting for an ack. A flooder that ignores SIGHUP,
/// already at its credit when killed, delivers less than the credit plus one chunk in all before its one `Exited`.
pub fn kill_drops_output_past_the_credit<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("stubborn-yes");
    h.pty
        .spawn(s.clone(), spec("sh", &["-c", "trap '' HUP; exec yes"]))
        .unwrap();
    let mut output = h.output_unacked(&s, OUTPUT_CREDIT);
    h.pty.kill(&s).unwrap();
    let (rest, code) = h.run_to_exit(&s);
    // `run_to_exit` decodes lossily, but `yes` prints ASCII.
    output.extend(rest.into_bytes());
    assert!(
        output.len() < OUTPUT_CREDIT + MAX_OUTPUT_CHUNK,
        "{} bytes delivered with nothing acked",
        output.len()
    );
    assert_eq!(code, None, "a killed process has no exit code");
    h.assert_quiet(QUIET);
}

/// Acknowledging more than is outstanding banks nothing: a flood acked far past what it sent still stops short of
/// the credit plus one chunk of new output.
pub fn acking_more_than_outstanding_banks_nothing<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("yes");
    h.pty.spawn(s.clone(), spec("yes", &[])).unwrap();
    let mut first = h.output_unacked(&s, OUTPUT_CREDIT);
    first.extend(h.output_within(&s, QUIET));

    h.pty.ack(&s, first.len() + 10 * OUTPUT_CREDIT).unwrap();
    let mut second = h.output_unacked(&s, OUTPUT_CREDIT);
    second.extend(h.output_within(&s, QUIET));
    assert!(
        second.len() < OUTPUT_CREDIT + MAX_OUTPUT_CHUNK,
        "{} bytes delivered after an excess ack",
        second.len()
    );

    h.pty.kill(&s).unwrap();
    h.run_to_exit(&s);
}
