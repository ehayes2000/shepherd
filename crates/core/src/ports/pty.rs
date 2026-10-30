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

/// Handing a [`Pty`]'s running sessions to another instance of it, so they outlive the instance (and the process
/// image) that spawned them: the host re-execs itself in place and adopts its PTYs (decision 0018).
///
/// [`export`](PtyHandoff::export) quiesces every live session and gives a [`Token`](PtyHandoff::Token) for each.
/// From then on the exporting instance reads, reaps and signals none of them, and treats them as unknown
/// ([`PtyError::UnknownSession`]); output they write meanwhile waits in the kernel for the adopter. Each token is then
/// either [`adopt`](PtyHandoff::adopt)ed by a fresh instance, which serves the session from then on exactly as if it
/// had spawned it (writes, resizes, kill, credit, and its one `Exited` with the exit code), or, if the handoff is called
/// off, [`resume`](PtyHandoff::resume) gives every exported session back to the exporter, unchanged.
pub trait PtyHandoff: Pty {
    /// Everything an adopter needs to take one session over.
    type Token: std::fmt::Debug + Send;

    /// Quiesces every live session and returns a token for each. Output read before the call has been sent to the
    /// sink when it returns. A session that exits meanwhile isn't exported: its `Exited` is sent as usual. On error
    /// nothing is exported (the sessions already quiesced are resumed).
    fn export(&mut self) -> Result<Vec<(SessionId, Self::Token)>, PtyError>;

    /// Calls off the last export: the exported sessions are served again as before it, and their tokens are void.
    fn resume(&mut self);

    /// Takes over an exported session as `session`. A session that can't be adopted (its terminal is gone, the token
    /// is unusable, or `session` is already live: [`PtyError::AlreadyExists`]) is killed, process group and all, so
    /// nothing is left running unattended, and the error says why; it gets no `Exited`. The one exception is a token
    /// whose process isn't a child of the adopter: whoever it is, it isn't signalled, and the error says so.
    fn adopt(&mut self, session: SessionId, token: Self::Token) -> Result<(), PtyError>;
}
