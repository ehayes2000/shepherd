//! Processes running in pseudo-terminals. The trait is synchronous: output and exits are pushed to the
//! [`PtyEventSink`] an implementation is constructed with, so core needs no async runtime.
//!
//! Output is flow-controlled by credit: a session whose unacknowledged output reaches [`OUTPUT_CREDIT`] isn't read
//! again until the receiver acknowledges some of it with [`Pty::ack`]. Until then the kernel's pty buffer fills and
//! the child blocks on its writes, as in a real terminal, so a flooding session holds at most about
//! `OUTPUT_CREDIT` bytes in the sink's queue.

use std::path::PathBuf;

use crate::ids::SessionId;

/// How many bytes of [`PtyEvent::Output`] a session may have unacknowledged before the port stops reading it. The
/// check happens before each read, so a session's unacknowledged output stays below
/// `OUTPUT_CREDIT + MAX_OUTPUT_CHUNK` while its child runs. Once the child has exited, what it left in the kernel's
/// buffer is still delivered before `Exited`, credit or not. After [`Pty::kill`], output past the credit is dropped
/// instead of waiting for an ack, so a kill always completes.
pub const OUTPUT_CREDIT: usize = 1024 * 1024;

/// The most bytes one [`PtyEvent::Output`] carries.
pub const MAX_OUTPUT_CHUNK: usize = 8 * 1024;

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
    /// Acknowledges `bytes` of `session`'s output as processed, returning that much credit (see [`OUTPUT_CREDIT`]).
    /// Acknowledging more than is outstanding is not an error; the excess is dropped, never banked.
    fn ack(&mut self, session: &SessionId, bytes: usize) -> Result<(), PtyError>;
}
