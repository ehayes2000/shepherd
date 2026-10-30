//! An in-memory `Commands` that runs scripted commands instead of a shell. A test registers what a script does by its
//! exact text; any other script behaves like a missing command (`sh: ...: command not found`, exit 127).
//!
//! The one thing it asks of the world is whether `cwd` is a directory, on the real filesystem, so the runtime's
//! scenarios use real worktree directories with it as they do with the PTY's cwd check.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.
//! [`FakeCommands::hold_next`] makes the next call block until the test releases it (or the code under test cancels
//! it), to stand in for a long hook. Calls run at the same time on different threads, as the real port's do.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL, RunId,
};

use super::hold::Gate;
pub use super::hold::Hold;
use super::hold::HoldSlot;

/// A scripted command's view of its run: what it was given, and what it does.
pub struct ScriptedCommand<'a> {
    spec: &'a CommandSpec,
    fake: &'a FakeCommands,
    gate: &'a Gate,
    output: Vec<u8>,
    end: End,
    forks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Exit(i32),
    Signal(i32),
    /// Never finishes on its own: it runs into its timeout.
    Hang,
    Cancelled,
}

impl ScriptedCommand<'_> {
    pub fn spec(&self) -> &CommandSpec {
        self.spec
    }

    /// The value of `name` in its env, as the port adds it.
    pub fn env(&self, name: &str) -> Option<&str> {
        self.spec
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Writes `bytes`. Only the tail is kept, as the real port keeps it.
    pub fn print(&mut self, bytes: impl AsRef<[u8]>) {
        self.output.extend_from_slice(bytes.as_ref());
        if self.output.len() > 2 * OUTPUT_TAIL {
            let cut = self.output.len() - OUTPUT_TAIL;
            self.output.drain(..cut);
        }
    }

    /// Exits with `code` after its output. Commands that call none of `exit`, `signal` or `hang` exit 0.
    pub fn exit(&mut self, code: i32) {
        self.end = End::Exit(code);
    }

    /// A signal ends it after its output.
    pub fn signal(&mut self, signal: i32) {
        self.end = End::Signal(signal);
    }

    /// It never finishes, so its timeout kills it, and everything it forked.
    pub fn hang(&mut self) {
        self.end = End::Hang;
    }

    /// It never finishes on its own: blocks until the run is cancelled, which kills it and everything it forked. A
    /// run not cancelled within a generous liveness bound panics.
    pub fn wait_for_cancel(&mut self) {
        if self.gate.wait("FakeCommands") {
            self.end = End::Cancelled;
        }
    }

    /// It starts a background child, which outlives it unless its group is killed (by its timeout or a cancel).
    pub fn fork_child(&mut self) {
        self.forks = true;
        self.fake.lock().child_alive = Some(true);
        self.fake.changed.notify_all();
    }
}

pub type Script = Arc<dyn Fn(&mut ScriptedCommand<'_>) + Send + Sync>;

#[derive(Default)]
struct State {
    scripts: HashMap<String, Script>,
    runs: Vec<CommandSpec>,
    fail_next: Option<CommandError>,
    /// Whether the last background child a script forked is alive. `None` before any did.
    child_alive: Option<bool>,
    /// Calls in progress.
    running: usize,
}

/// The runs a cancel can reach. Kept apart from `state`, so a held call holds no lock a cancel needs.
#[derive(Default)]
struct Flights {
    /// Each run in progress, and the gate it waits at while held or waiting for a cancel.
    running: HashMap<RunId, Arc<Gate>>,
    /// Runs cancelled before they started.
    cancelled_early: HashSet<RunId>,
}

#[derive(Clone, Default)]
pub struct FakeCommands {
    state: Arc<Mutex<State>>,
    /// Signalled when `state.child_alive` changes.
    changed: Arc<Condvar>,
    flights: Arc<Mutex<Flights>>,
    hold: HoldSlot,
}

impl FakeCommands {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers what running exactly `script` does.
    pub fn with_script(
        self,
        script: impl Into<String>,
        run: impl Fn(&mut ScriptedCommand<'_>) + Send + Sync + 'static,
    ) -> Self {
        self.add_script(script, run);
        self
    }

    /// Like [`FakeCommands::with_script`], on a shared handle.
    pub fn add_script(
        &self,
        script: impl Into<String>,
        run: impl Fn(&mut ScriptedCommand<'_>) + Send + Sync + 'static,
    ) {
        self.lock().scripts.insert(script.into(), Arc::new(run));
    }

    /// The next call fails with `error` and runs nothing.
    pub fn fail_next(&self, error: CommandError) {
        self.lock().fail_next = Some(error);
    }

    /// The next call blocks, before it does anything, until the returned [`Hold`] is released or dropped, or the run is
    /// cancelled (then it runs nothing and returns [`CommandExit::Cancelled`]). Calls after it are not held. A call
    /// held for longer than a generous liveness bound panics.
    pub fn hold_next(&self) -> Hold {
        self.hold.arm()
    }

    /// Every command that ran (or was asked to and failed, or was cancelled), in order.
    pub fn runs(&self) -> Vec<CommandSpec> {
        self.lock().runs.clone()
    }

    /// How many calls are in progress (held ones included).
    pub fn running(&self) -> usize {
        self.lock().running
    }

    /// Whether the last background child a script forked is still alive. `None` if none ever forked.
    pub fn child_alive(&self) -> Option<bool> {
        self.lock().child_alive
    }

    /// Blocks until some script has forked a background child, up to `limit` (a liveness bound). Whether one has.
    pub fn wait_child_forked(&self, limit: Duration) -> bool {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, limit, |s| s.child_alive.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.child_alive.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn flights(&self) -> MutexGuard<'_, Flights> {
        self.flights.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `command`, waiting at `gate` first if it is `held`.
    fn run_at(&self, gate: &Gate, held: bool, command: &CommandSpec) -> RunResult {
        if held && gate.wait("FakeCommands") {
            self.lock().runs.push(command.clone());
            return Ok(cancelled());
        }
        let script = {
            let mut s = self.lock();
            s.runs.push(command.clone());
            if let Some(error) = s.fail_next.take() {
                return Err(error);
            }
            s.scripts.get(&command.script).cloned()
        };
        if !std::fs::metadata(&command.cwd).is_ok_and(|m| m.is_dir()) {
            return Err(CommandError::NoSuchDir(command.cwd.clone()));
        }
        let mut run = ScriptedCommand {
            spec: command,
            fake: self,
            gate,
            output: Vec::new(),
            end: End::Exit(0),
            forks: false,
        };
        match script {
            Some(script) => script(&mut run),
            None => {
                run.print(format!("sh: {}: command not found\n", command.script));
                run.exit(127);
            }
        }
        let exit = match run.end {
            End::Exit(code) => CommandExit::Code(code),
            End::Signal(signal) => CommandExit::Signal(signal),
            End::Hang => CommandExit::TimedOut,
            End::Cancelled => CommandExit::Cancelled,
        };
        if run.forks && matches!(exit, CommandExit::TimedOut | CommandExit::Cancelled) {
            // The timeout or the cancel kills the whole group, children included.
            self.lock().child_alive = Some(false);
            self.changed.notify_all();
        }
        let start = run.output.len().saturating_sub(OUTPUT_TAIL);
        Ok(CommandOutput {
            exit,
            tail: run.output.split_off(start),
        })
    }
}

type RunResult = Result<CommandOutput, CommandError>;

fn cancelled() -> CommandOutput {
    CommandOutput {
        exit: CommandExit::Cancelled,
        tail: Vec::new(),
    }
}

impl Commands for FakeCommands {
    fn run(&self, id: RunId, command: &CommandSpec) -> RunResult {
        let held = self.hold.take();
        let gate = {
            let mut flights = self.flights();
            if flights.cancelled_early.remove(&id) {
                drop(flights);
                self.lock().runs.push(command.clone());
                return Ok(cancelled());
            }
            let gate = held.clone().unwrap_or_default();
            flights.running.insert(id, gate.clone());
            gate
        };
        self.lock().running += 1;
        let result = self.run_at(&gate, held.is_some(), command);
        self.flights().running.remove(&id);
        self.lock().running -= 1;
        result
    }

    /// A held call, or one waiting for its cancel, stops waiting at once. Any other running call is past the point
    /// where a cancel could reach it (the fake's scripts take no time), so a cancel then does nothing.
    fn cancel(&self, id: RunId) {
        let mut flights = self.flights();
        match flights.running.get(&id) {
            Some(gate) => gate.cancel(),
            None => {
                flights.cancelled_early.insert(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::contracts::commands::{
        CHATTY, COUNT_LINES, CommandsFixture, ENV_AND_CWD, ENV_VAR, FORK_AND_EXIT, FORK_AND_HANG,
        FORK_AND_WAIT, KILLED_BY_SIGNAL, PRINT_AND_EXIT, READS_STDIN,
    };

    /// The contract's scripts, scripted.
    fn contract_fake() -> FakeCommands {
        FakeCommands::new()
            .with_script(PRINT_AND_EXIT, |c| {
                c.print("outerr");
                c.exit(3);
            })
            .with_script(KILLED_BY_SIGNAL, |c| c.signal(15))
            .with_script(COUNT_LINES, |c| {
                for i in 0..1000 {
                    c.print(format!("{i:04}\n"));
                }
            })
            .with_script(ENV_AND_CWD, |c| {
                let line = format!(
                    "{}|{}",
                    c.env(ENV_VAR).unwrap_or_default(),
                    c.spec().cwd.display()
                );
                c.print(line);
            })
            // Stdin is always closed here, so `cat` reads nothing.
            .with_script(READS_STDIN, |c| c.print("done"))
            .with_script(FORK_AND_HANG, |c| {
                c.print("started\n");
                c.fork_child();
                c.hang();
            })
            .with_script(FORK_AND_EXIT, |c| c.fork_child())
            .with_script(FORK_AND_WAIT, |c| {
                c.print("started\n");
                c.fork_child();
                c.wait_for_cancel();
            })
            .with_script(CHATTY, |c| {
                c.print(b"0123456789abcde\n".repeat(8 * 1024 * 1024 / 16));
                c.print("end");
            })
    }

    pub struct FakeFixture {
        fake: FakeCommands,
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl FakeFixture {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = std::fs::canonicalize(tmp.path()).unwrap();
            Self {
                fake: contract_fake(),
                _tmp: tmp,
                dir,
            }
        }
    }

    impl CommandsFixture for FakeFixture {
        type Port = FakeCommands;

        fn port(&self) -> &FakeCommands {
            &self.fake
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        fn forked_child_alive(&self) -> Option<bool> {
            self.fake.child_alive()
        }

        fn wait_for_child(&self, limit: Duration) -> bool {
            self.fake.wait_child_forked(limit)
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::commands_contract!(FakeFixture::new());
    }

    fn spec(script: &str, cwd: PathBuf) -> CommandSpec {
        CommandSpec {
            script: script.to_owned(),
            cwd,
            env: Vec::new(),
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn an_unknown_script_is_a_missing_command() {
        let f = FakeFixture::new();
        let out = f
            .fake
            .run(RunId(1), &spec("frobnicate", f.dir.clone()))
            .unwrap();
        assert_eq!(out.exit, CommandExit::Code(127));
        assert_eq!(out.tail, b"sh: frobnicate: command not found\n");
    }

    #[test]
    fn fail_next_fails_only_the_next_call_and_runs_are_recorded() {
        let f = FakeFixture::new();
        f.fake.fail_next(CommandError::Failed("no fork".into()));
        let first = spec(PRINT_AND_EXIT, f.dir.clone());
        assert_eq!(
            f.fake.run(RunId(1), &first),
            Err(CommandError::Failed("no fork".into()))
        );
        assert_eq!(
            f.fake.run(RunId(2), &first).unwrap().exit,
            CommandExit::Code(3)
        );
        assert_eq!(f.fake.runs(), vec![first.clone(), first]);
    }

    #[test]
    fn a_held_call_waits_until_released() {
        let f = FakeFixture::new();
        let hold = f.fake.hold_next();
        let fake = f.fake.clone();
        let s = spec(PRINT_AND_EXIT, f.dir.clone());
        let call = std::thread::spawn(move || fake.run(RunId(1), &s));
        assert!(hold.wait_entered(Duration::from_secs(10)));
        assert!(!call.is_finished());
        assert_eq!(f.fake.running(), 1);
        hold.release();
        assert_eq!(call.join().unwrap().unwrap().exit, CommandExit::Code(3));
        assert_eq!(f.fake.running(), 0);
    }

    #[test]
    fn a_held_call_that_is_cancelled_runs_nothing() {
        let f = FakeFixture::new();
        let hold = f.fake.hold_next();
        let fake = f.fake.clone();
        let s = spec(FORK_AND_EXIT, f.dir.clone());
        let call = std::thread::spawn(move || fake.run(RunId(1), &s));
        assert!(hold.wait_entered(Duration::from_secs(10)));
        f.fake.cancel(RunId(1));
        assert!(hold.wait_cancelled(Duration::from_secs(10)));
        let out = call.join().unwrap().unwrap();
        assert_eq!(out.exit, CommandExit::Cancelled);
        assert_eq!(f.fake.child_alive(), None);
    }
}
