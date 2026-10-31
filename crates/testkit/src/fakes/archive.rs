//! An in-memory `Archive`: the sessions put, the snapshots taken, and the transcripts it can read. Nothing touches the
//! filesystem: a test (or the contract fixture) writes transcripts into it with [`FakeArchive::write_transcript`].
//!
//! A capture is refused, as the real archive refuses it, for a transcript path outside the agent home, or one that is
//! another agent session's; it fails for a session never put, or a transcript that isn't there.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.
//! [`FakeArchive::hold_next`] makes the next call block until the test releases it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use super::hold::Hold;
use super::hold::HoldSlot;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Archive, ArchiveError, ArchivedSession, CaptureRequest};

/// A transcript the fake can read: whose it is, and what it holds.
#[derive(Debug, Clone)]
struct Transcript {
    agent: AgentKind,
    agent_session_id: String,
    content: Vec<u8>,
}

/// A snapshot the fake took: what it was asked, and the transcript as it read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeSnapshot {
    pub request: CaptureRequest,
    pub transcript: Vec<u8>,
}

#[derive(Debug, Default)]
struct State {
    sessions: BTreeMap<SessionId, ArchivedSession>,
    transcripts: BTreeMap<PathBuf, Transcript>,
    snapshots: Vec<FakeSnapshot>,
    fail_next: Option<ArchiveError>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeArchive {
    state: Arc<Mutex<State>>,
    hold: HoldSlot,
}

impl FakeArchive {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts `content` at `path` as the transcript of `agent`'s session `agent_session_id`, replacing what was there.
    pub fn write_transcript(
        &self,
        path: impl Into<PathBuf>,
        agent: AgentKind,
        agent_session_id: &str,
        content: &[u8],
    ) {
        let transcript = Transcript {
            agent,
            agent_session_id: agent_session_id.to_owned(),
            content: content.to_vec(),
        };
        self.lock().transcripts.insert(path.into(), transcript);
    }

    /// Deletes the transcript at `path`.
    pub fn remove_transcript(&self, path: &Path) {
        self.lock().transcripts.remove(path);
    }

    /// The next call fails with `error` and stores nothing.
    pub fn fail_next(&self, error: ArchiveError) {
        self.lock().fail_next = Some(error);
    }

    /// The next call blocks, before it does anything, until the returned [`Hold`] is released or dropped.
    pub fn hold_next(&self) -> Hold {
        self.hold.arm()
    }

    /// Every session put, by id, as last put.
    pub fn sessions(&self) -> BTreeMap<SessionId, ArchivedSession> {
        self.lock().sessions.clone()
    }

    /// Every snapshot taken, oldest first.
    pub fn snapshots(&self) -> Vec<FakeSnapshot> {
        self.lock().snapshots.clone()
    }

    fn start(&self) -> Result<MutexGuard<'_, State>, ArchiveError> {
        self.hold.wait_if_held("FakeArchive");
        let mut s = self.lock();
        match s.fail_next.take() {
            Some(error) => Err(error),
            None => Ok(s),
        }
    }
}

impl Archive for FakeArchive {
    fn put_session(&self, session: &ArchivedSession) -> Result<(), ArchiveError> {
        let mut s = self.start()?;
        if !matches!(session.agent, AgentKind::ClaudeCode | AgentKind::Codex) {
            return Err(ArchiveError::Refused(format!(
                "the archive doesn't keep {} sessions",
                session.agent.as_str()
            )));
        }
        s.sessions.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn capture(&self, capture: &CaptureRequest) -> Result<(), ArchiveError> {
        let mut s = self.start()?;
        let session = s
            .sessions
            .get(&capture.session)
            .ok_or_else(|| {
                ArchiveError::Failed(format!("session {} is not in the archive", capture.session))
            })?
            .clone();
        let path = &capture.transcript_path;
        if !path.starts_with(&capture.agent_home) {
            return Err(ArchiveError::Refused(format!(
                "{} is outside the agent home {}",
                path.display(),
                capture.agent_home.display()
            )));
        }
        let transcript = s.transcripts.get(path).ok_or_else(|| {
            ArchiveError::Failed(format!("{}: No such file or directory", path.display()))
        })?;
        if transcript.agent != session.agent
            || transcript.agent_session_id != session.agent_session_id
        {
            return Err(ArchiveError::Refused(format!(
                "{} is not the transcript of its session {}",
                path.display(),
                session.agent_session_id
            )));
        }
        let snapshot = FakeSnapshot {
            request: capture.clone(),
            transcript: transcript.content.clone(),
        };
        s.snapshots.push(snapshot);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use shepherd_core::config::AgentKind;
    use shepherd_core::ids::SessionId;
    use shepherd_core::ports::{Archive, ArchiveError, CommitHash};

    use super::FakeArchive;
    use crate::contracts::archive::{ArchiveFixture, Snapshot};

    struct FakeFixture {
        fake: FakeArchive,
    }

    impl FakeFixture {
        fn new() -> Self {
            Self {
                fake: FakeArchive::new(),
            }
        }
    }

    impl ArchiveFixture for FakeFixture {
        type Port = FakeArchive;

        fn port(&self) -> &FakeArchive {
            &self.fake
        }

        fn agent_home(&self, agent: AgentKind) -> PathBuf {
            PathBuf::from(format!("/fake/home/{}", agent.as_str()))
        }

        fn write_transcript(&self, agent: AgentKind, id: &str, content: &[u8]) -> PathBuf {
            let path = self.agent_home(agent).join(format!("{id}.jsonl"));
            self.fake.write_transcript(&path, agent, id, content);
            path
        }

        fn remove_transcript(&self, path: &Path) {
            self.fake.remove_transcript(path);
        }

        fn outside_file(&self) -> PathBuf {
            PathBuf::from("/etc/passwd")
        }

        fn snapshots(&self, session: &SessionId) -> Vec<Snapshot> {
            self.fake
                .snapshots()
                .into_iter()
                .filter(|s| s.request.session == *session)
                .map(|s| Snapshot {
                    reason: s.request.reason,
                    head: s.request.head,
                    transcript: s.transcript,
                })
                .collect()
        }

        fn sessions_with_commit(&self, commit: &CommitHash) -> Vec<SessionId> {
            let mut found: Vec<SessionId> = self
                .fake
                .snapshots()
                .into_iter()
                .filter(|s| s.request.commits.iter().any(|(c, _)| c == commit))
                .map(|s| s.request.session)
                .collect();
            found.dedup();
            found
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::archive_contract!(FakeFixture::new());
    }

    #[test]
    fn fail_next_fails_one_call_and_stores_nothing() {
        let f = FakeFixture::new();
        let failure = ArchiveError::Failed("disk full".into());
        f.fake.fail_next(failure.clone());
        let session = crate::contracts::archive::claude_session();
        assert_eq!(f.fake.put_session(&session), Err(failure));
        assert!(f.fake.sessions().is_empty());
        assert_eq!(f.fake.put_session(&session), Ok(()));
    }

    #[test]
    fn a_held_call_waits_until_released() {
        let f = FakeFixture::new();
        let hold = f.fake.hold_next();
        let fake = f.fake.clone();
        let call = std::thread::spawn(move || {
            fake.put_session(&crate::contracts::archive::claude_session())
        });
        assert!(hold.wait_entered(Duration::from_secs(10)));
        assert!(!call.is_finished());
        hold.release();
        assert_eq!(call.join().ok(), Some(Ok(())));
    }
}
