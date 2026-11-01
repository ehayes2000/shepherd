//! Reading an agent's reply from its transcript (T47, decision 0022): what `shepherd prompt --wait` and `shepherd
//! reply` print. The real reader is the independent `shepherd-archive` crate, which reads the transcript through the
//! same path checks as a capture; core can't see its types, so these are core's own and the adapter in the `shepherd`
//! crate converts them. Calls block (they read a file): the host makes them on a worker thread.

use std::path::PathBuf;

use crate::config::AgentKind;

/// Where a turn starts: replies from before it don't count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TurnMarker {
    /// When the prompt that started the turn was recorded (its `session/prompted` entry's `ts`): the agent writes
    /// that turn's messages after it.
    pub at: jiff::Timestamp,
}

/// Which reply to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRequest {
    /// Claude Code or Codex: no other kind has a transcript.
    pub agent: AgentKind,
    /// The agent's home: Claude Code's config dir, or Codex's `CODEX_HOME`, as the session was launched with.
    pub agent_home: PathBuf,
    /// The `transcript_path` the agent's hooks gave, as given: never one shepherd computed.
    pub transcript_path: PathBuf,
    /// Only a reply written at or after this counts. `None`: the last reply in the transcript.
    pub since: Option<TurnMarker>,
}

/// The agent's last assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Its text blocks, joined by newlines.
    pub text: String,
    /// When the agent wrote it, if the transcript says.
    pub at: Option<jiff::Timestamp>,
}

/// Why no reply could be read. Its text is for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// The path isn't one the agent keeps a transcript at (outside its home, a symlink, past the size limit), or the
    /// agent kind keeps none.
    #[error("{0}")]
    Refused(String),
    /// The transcript doesn't exist.
    #[error("{0}")]
    Missing(String),
    /// Reading it failed.
    #[error("{0}")]
    Failed(String),
}

pub trait Transcripts {
    /// The agent's last assistant message with any text, written at or after `since` if given. `Ok(None)` when there
    /// is none (yet). Blocking: the host calls it on a worker, never the runtime thread.
    fn last_reply(&self, req: &ReplyRequest) -> Result<Option<Reply>, TranscriptError>;
}
