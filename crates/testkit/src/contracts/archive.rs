//! The `Archive` contract.
//!
//! Each case gets a fresh [`ArchiveFixture`]: an empty archive, and a home for each agent where the fixture writes
//! transcripts where that agent keeps them (Claude Code: `projects/<project>/<session>.jsonl`; Codex:
//! `sessions/YYYY/MM/DD/rollout-<time>-<session>.jsonl`). The fixture also reads back what the archive holds, which the
//! port itself never does.
//!
//! Run it with [`archive_contract!`](crate::archive_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Archive` contract case. `$fixture` is an expression building a fresh
/// [`ArchiveFixture`] for each test. New cases go in the list below.
#[macro_export]
macro_rules! archive_contract {
    ($fixture:expr) => {
        $crate::archive_contract!(@cases $fixture;
            a_capture_snapshots_the_transcript_as_it_is_then,
            a_codex_rollout_is_captured,
            put_session_twice_is_fine,
            put_session_again_moves_the_session_to_a_new_agent_session,
            a_capture_records_the_sessions_commits_and_head,
            a_capture_for_a_session_never_put_is_failed,
            a_transcript_outside_the_agent_home_is_refused,
            another_agent_sessions_transcript_is_refused,
            a_transcript_that_is_gone_is_failed,
            a_shell_session_is_refused,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::archive::$case(&$fixture);
            }
        )*
    };
}

use std::path::{Path, PathBuf};

use shepherd_core::acpp::Timestamp;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    Archive, ArchiveError, ArchivedSession, BranchName, CaptureReason, CaptureRequest, CommitHash,
    PatchId, RepoRoot,
};

/// What the archive holds of one snapshot, as a fixture reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub reason: CaptureReason,
    pub head: Option<CommitHash>,
    /// The bytes of its main transcript (Claude Code's session file, or Codex's rollout).
    pub transcript: Vec<u8>,
}

/// An empty archive, plus a home for each agent.
pub trait ArchiveFixture {
    type Port: Archive;

    fn port(&self) -> &Self::Port;
    /// `agent`'s home: an existing directory, where [`ArchiveFixture::write_transcript`] writes.
    fn agent_home(&self, agent: AgentKind) -> PathBuf;
    /// Writes `content` as the transcript of `agent`'s session `id`, where `agent` keeps it under its home (replacing
    /// what is there), and returns its path.
    fn write_transcript(&self, agent: AgentKind, id: &str, content: &[u8]) -> PathBuf;
    /// Deletes the transcript at `path`.
    fn remove_transcript(&self, path: &Path);
    /// An existing file outside every agent home.
    fn outside_file(&self) -> PathBuf;
    /// What the archive holds for `session`, oldest snapshot first.
    fn snapshots(&self, session: &SessionId) -> Vec<Snapshot>;
    /// The sessions the archive says made `commit`.
    fn sessions_with_commit(&self, commit: &CommitHash) -> Vec<SessionId>;
}

const SESSION: &str = "s_01";
const AGENT_SESSION: &str = "0a1b2c3d-4e5f-4061-8728-394a5b6c7d8e";

fn sid() -> SessionId {
    SessionId::new(SESSION)
}

fn hash(c: char) -> CommitHash {
    CommitHash::parse(&c.to_string().repeat(40)).unwrap()
}

/// The Claude Code session the cases put.
pub fn claude_session() -> ArchivedSession {
    session(AgentKind::ClaudeCode, AGENT_SESSION)
}

fn session(agent: AgentKind, agent_session: &str) -> ArchivedSession {
    ArchivedSession {
        id: sid(),
        repo: RepoRoot::new(hash('0')),
        machine: MachineId::new("ab12"),
        agent,
        agent_session_id: agent_session.to_owned(),
        name: Some("fix-login".to_owned()),
        branch: Some(BranchName::new("fix-login")),
        base: Some(hash('b')),
        cwd: PathBuf::from("/wt/app/fix-login"),
        created_at: Timestamp::parse("2026-10-31T09:00:00.000Z")
            .unwrap()
            .instant(),
    }
}

fn capture<F: ArchiveFixture>(
    f: &F,
    agent: AgentKind,
    transcript: PathBuf,
    reason: CaptureReason,
) -> CaptureRequest {
    CaptureRequest {
        session: sid(),
        at: Timestamp::parse("2026-10-31T10:00:00.000Z")
            .unwrap()
            .instant(),
        head: None,
        reason,
        commits: Vec::new(),
        agent_home: f.agent_home(agent),
        transcript_path: transcript,
    }
}

/// A Claude Code session put, with its transcript written as `content`. Returns the transcript's path.
fn claude<F: ArchiveFixture>(f: &F, content: &[u8]) -> PathBuf {
    f.port()
        .put_session(&session(AgentKind::ClaudeCode, AGENT_SESSION))
        .unwrap();
    f.write_transcript(AgentKind::ClaudeCode, AGENT_SESSION, content)
}

fn transcripts(snapshots: &[Snapshot]) -> Vec<&[u8]> {
    snapshots.iter().map(|s| s.transcript.as_slice()).collect()
}

pub fn a_capture_snapshots_the_transcript_as_it_is_then<F: ArchiveFixture>(f: &F) {
    let path = claude(f, b"{\"n\":1}\n");
    let first = capture(
        f,
        AgentKind::ClaudeCode,
        path.clone(),
        CaptureReason::TurnEnd,
    );
    f.port().capture(&first).unwrap();
    f.write_transcript(
        AgentKind::ClaudeCode,
        AGENT_SESSION,
        b"{\"n\":1}\n{\"n\":2}\n",
    );
    let second = capture(f, AgentKind::ClaudeCode, path, CaptureReason::Close);
    f.port().capture(&second).unwrap();
    let snapshots = f.snapshots(&sid());
    let reasons: Vec<CaptureReason> = snapshots.iter().map(|s| s.reason).collect();
    assert_eq!(reasons, [CaptureReason::TurnEnd, CaptureReason::Close]);
    assert_eq!(
        transcripts(&snapshots),
        [&b"{\"n\":1}\n"[..], &b"{\"n\":1}\n{\"n\":2}\n"[..]]
    );
}

pub fn a_codex_rollout_is_captured<F: ArchiveFixture>(f: &F) {
    let thread = "019a2b3c-4d5e-7f60-8172-93a4b5c6d7e8";
    f.port()
        .put_session(&session(AgentKind::Codex, thread))
        .unwrap();
    let path = f.write_transcript(AgentKind::Codex, thread, b"{\"type\":\"session_meta\"}\n");
    let request = capture(f, AgentKind::Codex, path, CaptureReason::BeforeRemove);
    f.port().capture(&request).unwrap();
    let snapshots = f.snapshots(&sid());
    assert_eq!(
        transcripts(&snapshots),
        [&b"{\"type\":\"session_meta\"}\n"[..]]
    );
    assert_eq!(snapshots[0].reason, CaptureReason::BeforeRemove);
}

pub fn put_session_twice_is_fine<F: ArchiveFixture>(f: &F) {
    let path = claude(f, b"{}\n");
    f.port()
        .put_session(&session(AgentKind::ClaudeCode, AGENT_SESSION))
        .unwrap();
    let request = capture(f, AgentKind::ClaudeCode, path, CaptureReason::TurnEnd);
    f.port().capture(&request).unwrap();
    assert_eq!(f.snapshots(&sid()).len(), 1);
}

/// `/clear` starts a new agent session in the same shepherd session: once it is put, its transcript is the one
/// captured.
pub fn put_session_again_moves_the_session_to_a_new_agent_session<F: ArchiveFixture>(f: &F) {
    claude(f, b"old\n");
    let fresh = "9f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a";
    f.port()
        .put_session(&session(AgentKind::ClaudeCode, fresh))
        .unwrap();
    let path = f.write_transcript(AgentKind::ClaudeCode, fresh, b"new\n");
    let request = capture(f, AgentKind::ClaudeCode, path, CaptureReason::TurnEnd);
    f.port().capture(&request).unwrap();
    assert_eq!(transcripts(&f.snapshots(&sid())), [&b"new\n"[..]]);
}

pub fn a_capture_records_the_sessions_commits_and_head<F: ArchiveFixture>(f: &F) {
    let path = claude(f, b"{}\n");
    let patch = PatchId::parse(&"f".repeat(40)).unwrap();
    let request = CaptureRequest {
        head: Some(hash('2')),
        commits: vec![(hash('1'), Some(patch)), (hash('2'), None)],
        ..capture(f, AgentKind::ClaudeCode, path, CaptureReason::TurnEnd)
    };
    f.port().capture(&request).unwrap();
    assert_eq!(f.sessions_with_commit(&hash('1')), [sid()]);
    assert_eq!(f.sessions_with_commit(&hash('2')), [sid()]);
    assert_eq!(f.sessions_with_commit(&hash('3')), []);
    assert_eq!(f.snapshots(&sid())[0].head, Some(hash('2')));
}

pub fn a_capture_for_a_session_never_put_is_failed<F: ArchiveFixture>(f: &F) {
    let path = f.write_transcript(AgentKind::ClaudeCode, AGENT_SESSION, b"{}\n");
    let request = capture(f, AgentKind::ClaudeCode, path, CaptureReason::TurnEnd);
    let result = f.port().capture(&request);
    assert!(matches!(result, Err(ArchiveError::Failed(_))), "{result:?}");
    assert_eq!(f.snapshots(&sid()), []);
}

pub fn a_transcript_outside_the_agent_home_is_refused<F: ArchiveFixture>(f: &F) {
    claude(f, b"{}\n");
    let request = capture(
        f,
        AgentKind::ClaudeCode,
        f.outside_file(),
        CaptureReason::BeforeRemove,
    );
    let result = f.port().capture(&request);
    assert!(
        matches!(result, Err(ArchiveError::Refused(_))),
        "{result:?}"
    );
    assert_eq!(f.snapshots(&sid()), []);
}

pub fn another_agent_sessions_transcript_is_refused<F: ArchiveFixture>(f: &F) {
    claude(f, b"mine\n");
    let theirs = f.write_transcript(
        AgentKind::ClaudeCode,
        "11111111-2222-4333-8444-555555555555",
        b"theirs\n",
    );
    let request = capture(f, AgentKind::ClaudeCode, theirs, CaptureReason::TurnEnd);
    let result = f.port().capture(&request);
    assert!(
        matches!(result, Err(ArchiveError::Refused(_))),
        "{result:?}"
    );
    assert_eq!(f.snapshots(&sid()), []);
}

pub fn a_transcript_that_is_gone_is_failed<F: ArchiveFixture>(f: &F) {
    let path = claude(f, b"{}\n");
    f.remove_transcript(&path);
    let request = capture(f, AgentKind::ClaudeCode, path, CaptureReason::BeforeRemove);
    let result = f.port().capture(&request);
    assert!(matches!(result, Err(ArchiveError::Failed(_))), "{result:?}");
    assert_eq!(f.snapshots(&sid()), []);
}

pub fn a_shell_session_is_refused<F: ArchiveFixture>(f: &F) {
    let result = f
        .port()
        .put_session(&session(AgentKind::Shell, AGENT_SESSION));
    assert!(
        matches!(result, Err(ArchiveError::Refused(_))),
        "{result:?}"
    );
}
