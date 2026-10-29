//! The `Pty` contract. Commands used here (`printf`, `cat`, `sh`, `stty`, `printenv`, `pwd`) run for real
//! on the adapter; the fake gets scripted versions of them (see `fakes::pty`).

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};

const TIMEOUT: Duration = Duration::from_secs(10);
/// How long to watch for stray events after an exit. Only used to assert that nothing else arrives.
const QUIET: Duration = Duration::from_millis(300);

pub fn suite<P: Pty>(make: impl Fn(Arc<dyn PtyEventSink>) -> P) {
    let h = || PtyHarness::new(&make);
    output_then_exit_code(h());
    exit_is_sent_once_and_last(h());
    writes_reach_the_child(h());
    resize_is_visible_to_the_child(h());
    kill_ends_the_process(h());
    env_is_honored(h());
    cwd_is_honored(h());
    exited_session_is_unknown(h());
    unknown_session_is_an_error(h());
    spawning_a_live_id_again_is_already_exists(h());
    spawning_a_missing_command_is_spawn_failed(h());
}

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

fn output_then_exit_code<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output, "hi");
    assert_eq!(code, Some(0));
}

fn exit_is_sent_once_and_last<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    h.assert_quiet(QUIET);
}

fn writes_reach_the_child<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    h.pty.spawn(s.clone(), spec("cat", &[])).unwrap();
    h.pty.write(&s, b"hello\n").unwrap();
    h.wait_for_output(&s, "hello");
    // ^D at the start of a line is EOF to cat; a clean exit proves it read what we wrote.
    h.pty.write(&s, b"\x04").unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, Some(0));
}

fn resize_is_visible_to_the_child<P: Pty>(mut h: PtyHarness<P>) {
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

fn kill_ends_the_process<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    h.pty.spawn(s.clone(), spec("cat", &[])).unwrap();
    h.pty.kill(&s).unwrap();
    let (_, code) = h.run_to_exit(&s);
    assert_eq!(code, None, "a killed process has no exit code");
}

fn env_is_honored<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printenv");
    let mut spec = spec("printenv", &["SHEPHERD_PTY_CONTRACT"]);
    spec.env
        .push(("SHEPHERD_PTY_CONTRACT".into(), "from-spec".into()));
    h.pty.spawn(s.clone(), spec).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output.trim_end(), "from-spec");
    assert_eq!(code, Some(0));
}

fn cwd_is_honored<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("pwd");
    let spec = spec("pwd", &[]);
    let cwd = spec.cwd.display().to_string();
    h.pty.spawn(s.clone(), spec).unwrap();
    let (output, code) = h.run_to_exit(&s);
    assert_eq!(output.trim_end(), cwd);
    assert_eq!(code, Some(0));
}

fn exited_session_is_unknown<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("printf");
    h.pty.spawn(s.clone(), spec("printf", &["hi"])).unwrap();
    h.run_to_exit(&s);
    assert_eq!(h.pty.write(&s, b"x"), Err(PtyError::UnknownSession));
    // The id is free again.
    h.pty.spawn(s.clone(), spec("printf", &["again"])).unwrap();
    assert_eq!(h.run_to_exit(&s), ("again".into(), Some(0)));
}

fn unknown_session_is_an_error<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("nobody");
    let size = PtySize { cols: 80, rows: 24 };
    assert_eq!(h.pty.write(&s, b"x"), Err(PtyError::UnknownSession));
    assert_eq!(h.pty.resize(&s, size), Err(PtyError::UnknownSession));
    assert_eq!(h.pty.kill(&s), Err(PtyError::UnknownSession));
}

fn spawning_a_live_id_again_is_already_exists<P: Pty>(mut h: PtyHarness<P>) {
    let s = sid("cat");
    h.pty.spawn(s.clone(), spec("cat", &[])).unwrap();
    assert_eq!(
        h.pty.spawn(s.clone(), spec("cat", &[])),
        Err(PtyError::AlreadyExists)
    );
    h.pty.kill(&s).unwrap();
    h.run_to_exit(&s);
}

fn spawning_a_missing_command_is_spawn_failed<P: Pty>(mut h: PtyHarness<P>) {
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
