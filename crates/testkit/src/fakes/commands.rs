//! An in-memory `Commands` that runs scripted commands instead of a shell. A test registers what a script does by its
//! exact text; any other script behaves like a missing command (`sh: ...: command not found`, exit 127).
//!
//! The one thing it asks of the world is whether `cwd` is a directory, on the real filesystem, so the runtime's
//! scenarios use real worktree directories with it as they do with the PTY's cwd check.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.
//! [`FakeCommands::hold_next`] makes the next call block until the test releases it, to stand in for a long hook.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL,
};

pub use super::hold::Hold;
use super::hold::HoldSlot;

/// A scripted command's view of its run: what it was given, and what it does.
pub struct ScriptedCommand<'a> {
    spec: &'a CommandSpec,
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

    pub fn print(&mut self, bytes: impl AsRef<[u8]>) {
        self.output.extend_from_slice(bytes.as_ref());
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

    /// It starts a background child, which outlives it unless the timeout kills its group.
    pub fn fork_child(&mut self) {
        self.forks = true;
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
}

#[derive(Clone, Default)]
pub struct FakeCommands {
    state: Arc<Mutex<State>>,
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

    /// The next call blocks, before it does anything, until the returned [`Hold`] is released or dropped. Calls after
    /// it are not held. A call held for longer than a generous liveness bound panics.
    pub fn hold_next(&self) -> Hold {
        self.hold.arm()
    }

    /// Every command that ran (or was asked to and failed), in order.
    pub fn runs(&self) -> Vec<CommandSpec> {
        self.lock().runs.clone()
    }

    /// Whether the last background child a script forked is still alive. `None` if none ever forked.
    pub fn child_alive(&self) -> Option<bool> {
        self.lock().child_alive
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Commands for FakeCommands {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandError> {
        self.hold.wait_if_held("FakeCommands");
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
        };
        if run.forks {
            // The timeout kills the whole group, children included.
            self.lock().child_alive = Some(exit != CommandExit::TimedOut);
        }
        let start = run.output.len().saturating_sub(OUTPUT_TAIL);
        Ok(CommandOutput {
            exit,
            tail: run.output.split_off(start),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::contracts::commands::{
        COUNT_LINES, CommandsFixture, ENV_AND_CWD, ENV_VAR, FORK_AND_EXIT, FORK_AND_HANG,
        KILLED_BY_SIGNAL, PRINT_AND_EXIT, READS_STDIN,
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
        let out = f.fake.run(&spec("frobnicate", f.dir.clone())).unwrap();
        assert_eq!(out.exit, CommandExit::Code(127));
        assert_eq!(out.tail, b"sh: frobnicate: command not found\n");
    }

    #[test]
    fn fail_next_fails_only_the_next_call_and_runs_are_recorded() {
        let f = FakeFixture::new();
        f.fake.fail_next(CommandError::Failed("no fork".into()));
        let first = spec(PRINT_AND_EXIT, f.dir.clone());
        assert_eq!(
            f.fake.run(&first),
            Err(CommandError::Failed("no fork".into()))
        );
        assert_eq!(f.fake.run(&first).unwrap().exit, CommandExit::Code(3));
        assert_eq!(f.fake.runs(), vec![first.clone(), first]);
    }

    #[test]
    fn a_held_call_waits_until_released() {
        let f = FakeFixture::new();
        let hold = f.fake.hold_next();
        let fake = f.fake.clone();
        let s = spec(PRINT_AND_EXIT, f.dir.clone());
        let call = std::thread::spawn(move || fake.run(&s));
        assert!(hold.wait_entered(Duration::from_secs(10)));
        assert!(!call.is_finished());
        hold.release();
        assert_eq!(call.join().unwrap().unwrap().exit, CommandExit::Code(3));
    }
}
