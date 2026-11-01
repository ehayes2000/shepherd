//! An in-memory `Transcripts`: the replies each transcript holds, written by a test with
//! [`FakeTranscripts::write_replies`]. Nothing touches the filesystem, and nothing is parsed: the real formats are the
//! archive crate's to read (its tests, on recorded fixtures).
//!
//! As the real reader does, it refuses a path outside the agent home, one that is another kind of agent's transcript,
//! and an agent kind that keeps none; a transcript that isn't there is missing.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd_core::config::AgentKind;
use shepherd_core::ports::{Reply, ReplyRequest, TranscriptError, Transcripts};

#[derive(Debug, Clone)]
struct Transcript {
    agent: AgentKind,
    /// Its replies, oldest first.
    replies: Vec<Reply>,
}

#[derive(Debug, Default)]
struct State {
    transcripts: BTreeMap<PathBuf, Transcript>,
    /// Every request, oldest first.
    asked: Vec<ReplyRequest>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeTranscripts {
    state: Arc<Mutex<State>>,
}

impl FakeTranscripts {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts a transcript of `agent`'s at `path` holding `replies` (oldest first), replacing what was there.
    pub fn write_replies(&self, path: impl Into<PathBuf>, agent: AgentKind, replies: &[Reply]) {
        let transcript = Transcript {
            agent,
            replies: replies.to_vec(),
        };
        self.lock().transcripts.insert(path.into(), transcript);
    }

    /// Adds `reply` to the transcript at `path`, making one of `agent`'s if there is none.
    pub fn add_reply(&self, path: impl Into<PathBuf>, agent: AgentKind, reply: Reply) {
        self.lock()
            .transcripts
            .entry(path.into())
            .or_insert(Transcript {
                agent,
                replies: Vec::new(),
            })
            .replies
            .push(reply);
    }

    /// Deletes the transcript at `path`.
    pub fn remove(&self, path: &Path) {
        self.lock().transcripts.remove(path);
    }

    /// Every request made, oldest first.
    pub fn asked(&self) -> Vec<ReplyRequest> {
        self.lock().asked.clone()
    }
}

impl Transcripts for FakeTranscripts {
    fn last_reply(&self, req: &ReplyRequest) -> Result<Option<Reply>, TranscriptError> {
        let mut s = self.lock();
        s.asked.push(req.clone());
        if !matches!(req.agent, AgentKind::ClaudeCode | AgentKind::Codex) {
            return Err(TranscriptError::Refused(format!(
                "{} keeps no transcript",
                req.agent.as_str()
            )));
        }
        let path = &req.transcript_path;
        if !path.starts_with(&req.agent_home) {
            return Err(TranscriptError::Refused(format!(
                "{} is outside the agent home {}",
                path.display(),
                req.agent_home.display()
            )));
        }
        let transcript = s
            .transcripts
            .get(path)
            .ok_or_else(|| TranscriptError::Missing(format!("{} doesn't exist", path.display())))?;
        if transcript.agent != req.agent {
            return Err(TranscriptError::Refused(format!(
                "{} is not where {} keeps a transcript",
                path.display(),
                req.agent.as_str()
            )));
        }
        let counts = |r: &&Reply| match (&req.since, &r.at) {
            (None, _) => true,
            (Some(since), Some(at)) => *at >= since.at,
            (Some(_), None) => false,
        };
        Ok(transcript.replies.iter().rev().find(counts).cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use shepherd_core::config::AgentKind;
    use shepherd_core::ports::Reply;

    use super::FakeTranscripts;
    use crate::contracts::transcripts::TranscriptsFixture;

    struct FakeFixture {
        fake: FakeTranscripts,
    }

    impl FakeFixture {
        fn new() -> Self {
            Self {
                fake: FakeTranscripts::new(),
            }
        }
    }

    impl TranscriptsFixture for FakeFixture {
        type Port = FakeTranscripts;

        fn port(&self) -> &FakeTranscripts {
            &self.fake
        }

        fn agent_home(&self, agent: AgentKind) -> PathBuf {
            PathBuf::from(format!("/fake/home/{}", agent.as_str()))
        }

        fn write_replies(&self, agent: AgentKind, id: &str, replies: &[Reply]) -> PathBuf {
            let path = self.agent_home(agent).join(format!("{id}.jsonl"));
            self.fake.write_replies(&path, agent, replies);
            path
        }

        fn outside_file(&self) -> PathBuf {
            PathBuf::from("/etc/passwd")
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::transcripts_contract!(FakeFixture::new());
    }
}
