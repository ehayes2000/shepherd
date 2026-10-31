//! The `Archive` contract on the real archive (`shepherd-archive`, through the adapter in this crate), in a tempdir:
//! the archive root and both agent homes are temp directories, never the real `~/.shepherd`, `~/.claude` or `~/.codex`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::str::FromStr;

use shepherd::archive::ShepherdArchive;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{CaptureReason, CommitHash};
use shepherd_testkit::contracts::archive::{ArchiveFixture, Snapshot};
use tempfile::TempDir;

struct RealFixture {
    dir: TempDir,
    port: ShepherdArchive,
}

impl RealFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for home in ["claude", "codex"] {
            std::fs::create_dir(dir.path().join(home)).unwrap();
        }
        std::fs::write(dir.path().join("outside.jsonl"), b"{}\n").unwrap();
        let port = ShepherdArchive::new(dir.path().join("archive"));
        Self { dir, port }
    }

    /// A second handle on the same archive, for reading back.
    fn reader(&self) -> shepherd_archive::Archive {
        shepherd_archive::Archive::open(&self.dir.path().join("archive")).unwrap()
    }
}

fn reason(r: shepherd_archive::SnapshotReason) -> CaptureReason {
    match r {
        shepherd_archive::SnapshotReason::TurnEnd => CaptureReason::TurnEnd,
        shepherd_archive::SnapshotReason::Close => CaptureReason::Close,
        shepherd_archive::SnapshotReason::BeforeRemove => CaptureReason::BeforeRemove,
        shepherd_archive::SnapshotReason::Manual => CaptureReason::Manual,
    }
}

impl ArchiveFixture for RealFixture {
    type Port = ShepherdArchive;

    fn port(&self) -> &ShepherdArchive {
        &self.port
    }

    fn agent_home(&self, agent: AgentKind) -> PathBuf {
        match agent {
            AgentKind::Codex => self.dir.path().join("codex"),
            _ => self.dir.path().join("claude"),
        }
    }

    /// Claude Code: `projects/<project>/<id>.jsonl`. Codex: `sessions/2026/10/31/rollout-<time>-<id>.jsonl`.
    fn write_transcript(&self, agent: AgentKind, id: &str, content: &[u8]) -> PathBuf {
        let home = self.agent_home(agent);
        let path = match agent {
            AgentKind::Codex => home
                .join("sessions/2026/10/31")
                .join(format!("rollout-2026-10-31T09-00-00-{id}.jsonl")),
            _ => home.join("projects/-wt-app").join(format!("{id}.jsonl")),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn remove_transcript(&self, path: &Path) {
        std::fs::remove_file(path).unwrap();
    }

    fn outside_file(&self) -> PathBuf {
        self.dir.path().join("outside.jsonl")
    }

    fn snapshots(&self, session: &SessionId) -> Vec<Snapshot> {
        let reader = self.reader();
        let id = shepherd_archive::SessionId::from_str(session.as_str()).unwrap();
        reader
            .snapshots(&id)
            .unwrap()
            .into_iter()
            .map(|s| {
                let files = reader.read(s.id).unwrap();
                let main = files
                    .into_iter()
                    .find(|f| f.file.role != shepherd_archive::FileRole::Subagent)
                    .expect("a main transcript or rollout");
                Snapshot {
                    reason: reason(s.reason),
                    head: s
                        .head_commit
                        .map(|c| CommitHash::parse(c.as_str()).unwrap()),
                    transcript: main.content,
                }
            })
            .collect()
    }

    fn sessions_with_commit(&self, commit: &CommitHash) -> Vec<SessionId> {
        let commit = shepherd_archive::CommitHash::from_str(commit.as_str()).unwrap();
        self.reader()
            .find_by_commit(&commit)
            .unwrap()
            .into_iter()
            .map(|hit| SessionId::new(hit.session.as_str()))
            .collect()
    }
}

mod contract {
    use super::RealFixture;

    shepherd_testkit::archive_contract!(RealFixture::new());
}

/// A transcript that is there but can't be read is a failure, never "missing": the worktree it ran in is kept, so the
/// conversation isn't lost.
#[test]
fn a_transcript_that_cannot_be_read_is_failed_not_missing() {
    use shepherd_core::ports::{Archive, ArchiveError, CaptureRequest};
    use std::os::unix::fs::PermissionsExt;
    let f = RealFixture::new();
    let session = shepherd_testkit::contracts::archive::claude_session();
    f.port.put_session(&session).unwrap();
    let path = f.write_transcript(AgentKind::ClaudeCode, &session.agent_session_id, b"{}\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = f.port.capture(&CaptureRequest {
        session: session.id.clone(),
        at: session.created_at,
        head: None,
        reason: CaptureReason::BeforeRemove,
        commits: Vec::new(),
        agent_home: f.agent_home(AgentKind::ClaudeCode),
        transcript_path: path.clone(),
    });
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(result, Err(ArchiveError::Failed(_))), "{result:?}");
    assert_eq!(f.snapshots(&session.id), []);
}

/// A Codex rollout compressed in place: the hook's path is gone, its `.zst` is there. Captured from the `.zst`; one
/// that can't be read is a failure, never "missing", though the path the hook named isn't there.
#[test]
fn a_compressed_codex_rollout_is_captured_and_an_unreadable_one_is_failed_not_missing() {
    use shepherd_core::ports::{Archive, ArchiveError, ArchivedSession, CaptureRequest};
    use std::os::unix::fs::PermissionsExt;
    const THREAD: &str = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
    let f = RealFixture::new();
    let session = ArchivedSession {
        agent: AgentKind::Codex,
        agent_session_id: THREAD.to_owned(),
        ..shepherd_testkit::contracts::archive::claude_session()
    };
    f.port.put_session(&session).unwrap();
    // The archive crate's provisional fixture: that thread's rollout, compressed as Codex does it.
    let rollout = format!("sessions/2026/10/30/rollout-2026-10-30T10-00-00-{THREAD}.jsonl");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../archive/tests/fixtures");
    let path = f.agent_home(AgentKind::Codex).join(&rollout);
    let compressed = PathBuf::from(format!("{}.zst", path.display()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(
        fixtures
            .join("codex-compressed/home")
            .join(format!("{rollout}.zst")),
        &compressed,
    )
    .unwrap();
    let plain = std::fs::read(fixtures.join("codex/home").join(&rollout)).unwrap();
    let capture = |reason| CaptureRequest {
        session: session.id.clone(),
        at: session.created_at,
        head: None,
        reason,
        commits: Vec::new(),
        agent_home: f.agent_home(AgentKind::Codex),
        transcript_path: path.clone(),
    };
    assert_eq!(f.port.capture(&capture(CaptureReason::TurnEnd)), Ok(()));
    let snapshots = f.snapshots(&session.id);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].transcript, plain);

    std::fs::set_permissions(&compressed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = f.port.capture(&capture(CaptureReason::BeforeRemove));
    std::fs::set_permissions(&compressed, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(result, Err(ArchiveError::Failed(_))), "{result:?}");
    assert_eq!(f.snapshots(&session.id).len(), 1);
}

/// The archive is opened on first use: an archive that can't be opened fails each call (and is tried again on the
/// next), without stopping the host from starting.
#[test]
fn an_archive_that_cannot_be_opened_fails_its_calls() {
    use shepherd_core::ports::{Archive, ArchiveError};
    let f = RealFixture::new();
    let blocked = f.dir.path().join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let port = ShepherdArchive::new(blocked.join("archive"));
    let session = shepherd_testkit::contracts::archive::claude_session();
    let result = port.put_session(&session);
    assert!(matches!(result, Err(ArchiveError::Failed(_))), "{result:?}");
}
