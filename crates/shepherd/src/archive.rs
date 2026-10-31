//! The `Archive` port on the `shepherd-archive` crate (decision 0020). Core can't see that crate's types, so this
//! converts core's into the crate's, each validated by the crate, and the crate's errors into core's: a capture the
//! crate refused (a path outside the agent's own place, past its limits) or a value it can't record is
//! [`ArchiveError::Refused`]; anything else (reading a file, the index, an unknown session) is [`ArchiveError::Failed`].
//!
//! The archive is opened on first use, so a host whose archive can't be opened (say, an index from a newer shepherd)
//! still starts: each call fails, saying why, and the next call tries again. Calls block; the host makes them on a
//! worker thread.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};

use shepherd_archive as archive;
use shepherd_core::config::AgentKind;
use shepherd_core::ports::{
    Archive, ArchiveError, ArchivedSession, CaptureReason, CaptureRequest, CommitHash, PatchId,
};

/// The archive under a root (`~/.shepherd/archive` in production).
pub struct ShepherdArchive {
    root: PathBuf,
    open: Mutex<Option<Arc<archive::Archive>>>,
}

impl ShepherdArchive {
    /// The archive under `root`, opened (and made, if missing) on first use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            open: Mutex::new(None),
        }
    }

    fn archive(&self) -> Result<Arc<archive::Archive>, ArchiveError> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(archive) = open.as_ref() {
            return Ok(archive.clone());
        }
        let opened = archive::Archive::open(&self.root).map_err(|e| {
            ArchiveError::Failed(format!(
                "cannot open the archive in {}: {e}",
                self.root.display()
            ))
        })?;
        let opened = Arc::new(opened);
        *open = Some(opened.clone());
        Ok(opened)
    }
}

/// A value the archive won't record.
fn refused(e: archive::BadValue) -> ArchiveError {
    ArchiveError::Refused(e.to_string())
}

fn failed(e: archive::ArchiveError) -> ArchiveError {
    match e {
        archive::ArchiveError::Capture(archive::CaptureError::Io { path, message }) => {
            ArchiveError::Failed(format!("{}: {message}", path.display()))
        }
        archive::ArchiveError::Capture(refusal) => ArchiveError::Refused(refusal.to_string()),
        other => ArchiveError::Failed(other.to_string()),
    }
}

fn agent(kind: AgentKind) -> Result<archive::AgentKind, ArchiveError> {
    match kind {
        AgentKind::ClaudeCode => Ok(archive::AgentKind::Claude),
        AgentKind::Codex => Ok(archive::AgentKind::Codex),
        AgentKind::Shell => Err(ArchiveError::Refused(format!(
            "the archive doesn't keep {} sessions",
            kind.as_str()
        ))),
    }
}

fn reason(reason: CaptureReason) -> archive::SnapshotReason {
    match reason {
        CaptureReason::TurnEnd => archive::SnapshotReason::TurnEnd,
        CaptureReason::Close => archive::SnapshotReason::Close,
        CaptureReason::BeforeRemove => archive::SnapshotReason::BeforeRemove,
        CaptureReason::Manual => archive::SnapshotReason::Manual,
    }
}

fn commit(c: &CommitHash) -> Result<archive::CommitHash, ArchiveError> {
    archive::CommitHash::from_str(c.as_str()).map_err(refused)
}

fn patch(p: &PatchId) -> Result<archive::PatchId, ArchiveError> {
    archive::PatchId::from_str(p.as_str()).map_err(refused)
}

fn session_id(id: &shepherd_core::ids::SessionId) -> Result<archive::SessionId, ArchiveError> {
    archive::SessionId::from_str(id.as_str()).map_err(refused)
}

impl Archive for ShepherdArchive {
    fn put_session(&self, s: &ArchivedSession) -> Result<(), ArchiveError> {
        let session = archive::Session {
            id: session_id(&s.id)?,
            repo: archive::RepoId::from_root_commit(commit(s.repo.commit())?),
            machine: archive::MachineId::from_str(s.machine.as_str()).map_err(refused)?,
            agent: agent(s.agent)?,
            agent_session_id: archive::AgentSessionId::from_str(&s.agent_session_id)
                .map_err(refused)?,
            name: s.name.clone(),
            branch: s
                .branch
                .as_ref()
                .map(|b| archive::Branch::from_str(b.as_str()))
                .transpose()
                .map_err(refused)?,
            pr: None,
            base_commit: s.base.as_ref().map(commit).transpose()?,
            cwd: s.cwd.clone(),
            created_at: s.created_at,
            archived_at: None,
        };
        self.archive()?.put_session(&session).map_err(failed)
    }

    fn capture(&self, c: &CaptureRequest) -> Result<(), ArchiveError> {
        let commits = c
            .commits
            .iter()
            .map(|(c, p)| {
                Ok(archive::SessionCommit {
                    commit: commit(c)?,
                    patch_id: p.as_ref().map(patch).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, ArchiveError>>()?;
        let capture = archive::Capture {
            session: session_id(&c.session)?,
            captured_at: c.at,
            head_commit: c.head.as_ref().map(commit).transpose()?,
            reason: reason(c.reason),
            commits,
            agent_home: &c.agent_home,
            transcript_path: &c.transcript_path,
            limits: archive::CaptureLimits::default(),
        };
        self.archive()?.capture(&capture).map(drop).map_err(failed)
    }
}
