//! Running a user's shell commands to completion, e.g. a workspace's worktree hooks.
//!
//! The trait is synchronous: [`Commands::run`] returns when the command has finished or its timeout has passed. The
//! runtime calls it on a worker thread, never on its loop.
//!
//! A command is a script for `/bin/sh -c` (portable, not the user's login shell). It runs in `cwd` with its stdin
//! closed, in a process group of its own, with the inherited environment plus `env`. It never gets a terminal, so
//! nothing it runs can prompt. One that runs past its timeout is killed with its whole group, so nothing it started
//! is left behind, and the call returns once the group is gone. Its stdout and stderr are kept together, interleaved
//! as written, and only the last [`OUTPUT_TAIL`] bytes come back. A background process it leaves running after it
//! exits normally is left alone and doesn't hold the call open.

use std::path::PathBuf;
use std::time::Duration;

/// The most bytes of a command's output [`Commands::run`] returns: the end of it.
pub const OUTPUT_TAIL: usize = 4096;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The script, run as `/bin/sh -c <script>`.
    pub script: String,
    /// Where it runs. Must be an existing directory.
    pub cwd: PathBuf,
    /// Added to the inherited environment, overriding variables with the same name.
    pub env: Vec<(String, String)>,
    /// How long it may run before it and everything it started are killed.
    pub timeout: Duration,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandExit {
    /// It exited with this code.
    Code(i32),
    /// A signal ended it (one it didn't get from the timeout).
    Signal(i32),
    /// It ran past its timeout and was killed, with its process group.
    TimedOut,
}

/// A command that ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit: CommandExit,
    /// The last [`OUTPUT_TAIL`] bytes (at most) of its stdout and stderr together, as written. Raw bytes: not
    /// necessarily UTF-8, and they may start in the middle of a character.
    pub tail: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// `cwd` is not an existing directory.
    #[error("{0} is not an existing directory")]
    NoSuchDir(PathBuf),
    /// It couldn't be started or waited for.
    #[error("{0}")]
    Failed(String),
}

pub trait Commands {
    /// Runs `command` to completion, or until its timeout. A command that ran is `Ok` whatever its exit; only one that
    /// couldn't run is an error.
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandError>;
}
