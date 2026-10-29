//! Processes running in pseudo-terminals. The trait is synchronous: output and exits are pushed to the
//! [`PtyEventSink`] an implementation is constructed with, so core needs no async runtime.

use std::path::PathBuf;

use crate::ids::SessionId;

/// What to run in a new PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Added to the inherited environment, overriding variables with the same name.
    pub env: Vec<(String, String)>,
    pub size: PtySize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

/// Pushed by a [`Pty`] as things happen. Each session gets exactly one `Exited`, after all its `Output`.
/// Once `Exited` is sent the session is unknown to the port, and its id may be spawned again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Output {
        session: SessionId,
        bytes: Vec<u8>,
    },
    /// `code` is `None` when the process was ended by a signal, including by [`Pty::kill`].
    Exited {
        session: SessionId,
        code: Option<i32>,
    },
}

pub trait PtyEventSink: Send + Sync + 'static {
    fn send(&self, event: PtyEvent);
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PtyError {
    #[error("unknown session")]
    UnknownSession,
    #[error("session already exists")]
    AlreadyExists,
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
    #[error("pty io: {0}")]
    Io(String),
}

pub trait Pty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError>;
    fn write(&mut self, session: &SessionId, bytes: &[u8]) -> Result<(), PtyError>;
    fn resize(&mut self, session: &SessionId, size: PtySize) -> Result<(), PtyError>;
    /// Ends the process. Its `Exited` arrives through the sink like any other exit.
    fn kill(&mut self, session: &SessionId) -> Result<(), PtyError>;
}
