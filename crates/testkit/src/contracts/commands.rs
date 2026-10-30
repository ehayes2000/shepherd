//! The `Commands` contract. The scripts used here ([`PRINT_AND_EXIT`], ...) run for real under `/bin/sh` on the
//! adapter; the fake gets scripted versions of them (see `fakes::commands`).
//!
//! Run it with [`commands_contract!`](crate::commands_contract), which expands to one `#[test]` per case. Add every
//! new case to the macro's list.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Commands` contract case. `$fixture` is an expression building a fresh
/// [`CommandsFixture`] for each test, e.g. `shepherd_testkit::commands_contract!(ShellFixture::new());`.
#[macro_export]
macro_rules! commands_contract {
    ($fixture:expr) => {
        $crate::commands_contract!(@cases $fixture;
            exit_code_and_interleaved_output,
            a_signal_is_reported,
            output_is_capped_to_its_tail,
            env_and_cwd_are_honored,
            stdin_is_closed,
            a_missing_cwd_is_an_error,
            a_file_as_cwd_is_an_error,
            timeout_kills_the_whole_group,
            a_background_child_does_not_hold_it_open,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::commands::$case(&$fixture);
            }
        )*
    };
}

use std::path::Path;
use std::time::Duration;

use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL,
};

/// Prints `out` to stdout, then `err` to stderr, and exits 3.
pub const PRINT_AND_EXIT: &str = "printf out; printf err >&2; exit 3";
/// Ends itself with SIGTERM.
pub const KILLED_BY_SIGNAL: &str = "kill -TERM $$";
/// Prints `0000\n` to `0999\n`: 5000 bytes.
pub const COUNT_LINES: &str =
    "i=0; while [ $i -lt 1000 ]; do printf '%04d\\n' $i; i=$((i+1)); done";
/// Prints `$T23_CONTRACT_VAR|<its physical cwd>`.
pub const ENV_AND_CWD: &str = "printf '%s|%s' \"$T23_CONTRACT_VAR\" \"$(pwd -P)\"";
/// The variable [`ENV_AND_CWD`] prints.
pub const ENV_VAR: &str = "T23_CONTRACT_VAR";
/// Reads stdin to its end, then prints `done`.
pub const READS_STDIN: &str = "cat; printf done";
/// Prints `started`, starts a `sleep` in the background, records its pid in `child.pid`, and waits for it: it never
/// finishes on its own.
pub const FORK_AND_HANG: &str = "echo started; sleep 300 & echo $! > child.pid; wait";
/// Starts a `sleep` in the background, records its pid in `child.pid`, and exits 0 without waiting for it.
pub const FORK_AND_EXIT: &str = "sleep 300 & echo $! > child.pid";

/// A timeout no case here comes near, except on purpose.
const GENEROUS: Duration = Duration::from_secs(60);

/// A port, and a directory to run commands in.
pub trait CommandsFixture {
    type Port: Commands;
    fn port(&self) -> &Self::Port;
    /// An existing directory, with symlinks resolved, that commands may write in. Empty at first.
    fn dir(&self) -> &Path;
    /// Whether the background child of [`FORK_AND_HANG`] or [`FORK_AND_EXIT`] is still alive. `None` if the script
    /// never got as far as recording it.
    fn forked_child_alive(&self) -> Option<bool>;
}

fn spec<F: CommandsFixture>(f: &F, script: &str, timeout: Duration) -> CommandSpec {
    CommandSpec {
        script: script.to_owned(),
        cwd: f.dir().to_path_buf(),
        env: Vec::new(),
        timeout,
    }
}

fn run<F: CommandsFixture>(f: &F, spec: &CommandSpec) -> CommandOutput {
    f.port()
        .run(spec)
        .unwrap_or_else(|e| panic!("running {:?}: {e}", spec.script))
}

pub fn exit_code_and_interleaved_output<F: CommandsFixture>(f: &F) {
    let out = run(f, &spec(f, PRINT_AND_EXIT, GENEROUS));
    assert_eq!(out.exit, CommandExit::Code(3));
    assert_eq!(out.tail, b"outerr");
}

pub fn a_signal_is_reported<F: CommandsFixture>(f: &F) {
    let out = run(f, &spec(f, KILLED_BY_SIGNAL, GENEROUS));
    assert_eq!(out.exit, CommandExit::Signal(15));
}

pub fn output_is_capped_to_its_tail<F: CommandsFixture>(f: &F) {
    let out = run(f, &spec(f, COUNT_LINES, GENEROUS));
    assert_eq!(out.exit, CommandExit::Code(0));
    let all: Vec<u8> = (0..1000)
        .flat_map(|i| format!("{i:04}\n").into_bytes())
        .collect();
    assert_eq!(out.tail.len(), OUTPUT_TAIL);
    assert_eq!(out.tail, all[all.len() - OUTPUT_TAIL..]);
}

pub fn env_and_cwd_are_honored<F: CommandsFixture>(f: &F) {
    let mut spec = spec(f, ENV_AND_CWD, GENEROUS);
    spec.env = vec![(ENV_VAR.to_owned(), "a value with spaces".to_owned())];
    let out = run(f, &spec);
    assert_eq!(out.exit, CommandExit::Code(0));
    let expected = format!("a value with spaces|{}", f.dir().display());
    assert_eq!(String::from_utf8_lossy(&out.tail), expected);
}

pub fn stdin_is_closed<F: CommandsFixture>(f: &F) {
    let out = run(f, &spec(f, READS_STDIN, GENEROUS));
    assert_eq!(out.exit, CommandExit::Code(0));
    assert_eq!(out.tail, b"done");
}

pub fn a_missing_cwd_is_an_error<F: CommandsFixture>(f: &F) {
    let mut spec = spec(f, PRINT_AND_EXIT, GENEROUS);
    spec.cwd = f.dir().join("missing");
    assert_eq!(
        f.port().run(&spec),
        Err(CommandError::NoSuchDir(spec.cwd.clone()))
    );
}

pub fn a_file_as_cwd_is_an_error<F: CommandsFixture>(f: &F) {
    let file = f.dir().join("a-file");
    std::fs::write(&file, b"").unwrap();
    let mut spec = spec(f, PRINT_AND_EXIT, GENEROUS);
    spec.cwd = file.clone();
    assert_eq!(f.port().run(&spec), Err(CommandError::NoSuchDir(file)));
}

/// The command never finishes, so any timeout is passed. A short one may pass before the script has even recorded its
/// child, which proves nothing about the child, so then it tries again with a longer one.
pub fn timeout_kills_the_whole_group<F: CommandsFixture>(f: &F) {
    let mut timeout = Duration::from_millis(200);
    loop {
        let out = run(f, &spec(f, FORK_AND_HANG, timeout));
        assert_eq!(out.exit, CommandExit::TimedOut);
        match f.forked_child_alive() {
            Some(alive) => {
                assert!(!alive, "the hook's background child outlived the timeout");
                assert_eq!(out.tail, b"started\n");
                return;
            }
            None if timeout < GENEROUS => timeout *= 2,
            None => panic!("the script never recorded its child, even with {timeout:?}"),
        }
    }
}

/// With a generous timeout, returning at all shows the call didn't wait for the child.
pub fn a_background_child_does_not_hold_it_open<F: CommandsFixture>(f: &F) {
    let out = run(f, &spec(f, FORK_AND_EXIT, GENEROUS));
    assert_eq!(out.exit, CommandExit::Code(0));
    assert_eq!(f.forked_child_alive(), Some(true));
}
