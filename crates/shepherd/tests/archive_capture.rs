//! The host archives agent conversations (T42): a Claude Code session in a worktree is captured when its agent ends a
//! turn, when it closes, and before its worktree is removed, which waits for that capture. A host runtime on fake
//! ports, the fake `Archive` and `Worktrees` on their worker threads. The workspace and the worktree are real
//! directories in a tempdir (the session's PTY spawns there); the transcript lives in the fake archive, under the
//! harness's Claude home. Never the real `claude`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use shepherd::host::{CREATE_CLEANUP_GRACE, RuntimeEvent, SHUTDOWN_GRACE};
use shepherd_core::acpp::{Frame, Seq, ShepherdFrame};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{CaptureReason, CommitHash, EventLog, LogId, RepoRef, Worktrees};
use shepherd_core::wire::{
    BuildId, ClientRequest, CloseWorktree, ExeDigest, Hello, HookFrame, HostUpgrade,
    PROTOCOL_VERSION, SessionClose, SessionNew, UpgradeAnswer,
};
use shepherd_testkit::fakes::archive::FakeArchive;
use shepherd_testkit::fakes::pty::ProgramInput;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A liveness bound for a held call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);
/// The agent's own id for its session.
const AGENT_SESSION: &str = "0a1b2c3d-4e5f-4061-8728-394a5b6c7d8e";

fn transcript() -> PathBuf {
    PathBuf::from(CLAUDE_HOME)
        .join("projects/-wt-app-claude-1")
        .join(format!("{AGENT_SESSION}.jsonl"))
}

/// A host whose workspace is `<tempdir>/app`, a subscribed client, and a Claude Code session running in its worktree,
/// whose `SessionStart` hook named its transcript. Worktrees go under `<tempdir>/worktrees`.
struct Setup {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    worktrees: FakeWorktrees,
    archive: FakeArchive,
    h: HostHarness,
    client: FakeClient,
    id: SessionId,
    path: PathBuf,
    /// Everything typed into the fake `claude`.
    typed: Arc<Mutex<Vec<u8>>>,
}

impl Setup {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let app = base.join("app");
        let root = base.join("worktrees");
        std::fs::create_dir(&app).unwrap();
        let worktrees = FakeWorktrees::new(&root);
        worktrees.add_repo(&app, "main");
        let archive = FakeArchive::new();
        let typed = Arc::new(Mutex::new(Vec::new()));
        let record = typed.clone();
        let claude: Program = Arc::new(move |input, _| {
            if let ProgramInput::Stdin(bytes) = input {
                record.lock().unwrap().extend_from_slice(bytes);
            }
        });
        // A newer build is installed, so an upgrade would swap (unless something keeps the host busy).
        let exe = base.join("shepherd");
        std::fs::write(&exe, b"a newer build").unwrap();
        let running = BuildId {
            version: "0.1.0".into(),
            digest: ExeDigest::from_bytes([1; 32]),
        };
        let mut h = HostHarness::builder()
            .worktrees(worktrees.clone())
            .archive(archive.clone())
            .program("claude", claude)
            .build(running, &exe)
            .start();
        let mut client = h.connect();
        client.request(ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("laptop"),
            cwd: Some(app.to_str().unwrap().to_owned()),
            build: None,
        }));
        h.step();
        client.subscribe_all();
        h.step();
        client.take();
        // The agent spawns in the worktree, so it must exist where git would have made it.
        let path = root.join("app").join("claude-1");
        std::fs::create_dir_all(&path).unwrap();
        let req = client.request(ClientRequest::SessionNew(SessionNew {
            profile: Some("claude".to_owned()),
            ..SessionNew::default()
        }));
        h.step();
        let id = new_session_id(&client.take(), &req);
        assert!(h.runtime().pty().is_running(&id));
        archive.write_transcript(
            transcript(),
            AgentKind::ClaudeCode,
            AGENT_SESSION,
            b"{\"type\":\"user\"}\n",
        );
        let mut s = Self {
            _tmp: tmp,
            app,
            worktrees,
            archive,
            h,
            client,
            id,
            path,
            typed,
        };
        s.hook(json!({
            "hook_event_name": "SessionStart",
            "session_id": AGENT_SESSION,
            "transcript_path": transcript(),
            "source": "startup",
        }));
        s
    }

    /// The agent runs a hook with `payload`; the host handles it, and what follows.
    fn hook(&mut self, payload: Value) {
        self.send_hook(payload);
        self.h.step();
    }

    fn send_hook(&mut self, payload: Value) {
        self.client
            .request(ClientRequest::HookFrame(Box::new(HookFrame {
                session: self.id.clone(),
                agent: AgentKind::ClaudeCode,
                payload,
            })));
    }

    fn stop(&mut self) -> Value {
        json!({ "hook_event_name": "Stop", "session_id": AGENT_SESSION })
    }

    fn close(&mut self) {
        self.client
            .request(ClientRequest::SessionClose(SessionClose {
                session: self.id.clone(),
                worktree: CloseWorktree::IfSafe,
            }));
    }

    /// Whether the session's worktree is still one of the repo's.
    fn listed(&self) -> bool {
        self.worktrees
            .list_managed(&RepoRef::new(&self.app))
            .unwrap()
            .iter()
            .any(|w| w.path.as_path() == self.path)
    }

    fn reasons(&self) -> Vec<CaptureReason> {
        self.archive
            .snapshots()
            .iter()
            .map(|s| s.request.reason)
            .collect()
    }

    fn closed(&self) -> shepherd_core::acpp::SessionClosed {
        let log = self
            .h
            .log()
            .read(LogId::Session(self.id.clone()), Seq::ZERO)
            .unwrap();
        let last = log.iter().rev().find_map(|env| match env.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::SessionClosed(closed) => Some(closed.clone()),
                _ => None,
            },
            _ => None,
        });
        last.expect("the session is closed")
    }

    /// Steps until `hold`'s call has started and is waiting.
    fn wait_held(&mut self, hold: &shepherd_testkit::fakes::hold::Hold) {
        self.h.poll();
        assert!(hold.wait_entered(LIMIT), "the archive was never called");
    }
}

fn hash(s: &str) -> CommitHash {
    CommitHash::parse(s).unwrap()
}

#[test]
fn a_turn_end_snapshots_the_conversation_with_the_sessions_commits() {
    let mut s = Setup::new();
    s.worktrees.commit(&s.path);
    let stop = s.stop();
    s.hook(stop);
    let snapshots = s.archive.snapshots();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.request.reason, CaptureReason::TurnEnd);
    assert_eq!(snapshot.transcript, b"{\"type\":\"user\"}\n");
    assert_eq!(snapshot.request.transcript_path, transcript());
    assert_eq!(snapshot.request.agent_home, PathBuf::from(CLAUDE_HOME));
    let head = hash(&s.worktrees.head(&s.path));
    let commits: Vec<&CommitHash> = snapshot.request.commits.iter().map(|(c, _)| c).collect();
    assert_eq!(commits, [&head]);
    assert!(snapshot.request.commits[0].1.is_some(), "with its patch-id");
    assert_eq!(snapshot.request.head, Some(head));
    let recorded = &s.archive.sessions()[&s.id];
    let root = s.worktrees.root_commit(&RepoRef::new(&s.app)).unwrap();
    assert_eq!(recorded.repo, root);
    assert_eq!(recorded.agent, AgentKind::ClaudeCode);
    assert_eq!(recorded.agent_session_id, AGENT_SESSION);
    assert_eq!(recorded.machine, s.h.machine());
    assert_eq!(recorded.cwd, s.path);
    assert!(recorded.base.is_some());
}

#[test]
fn a_hook_that_is_not_a_turn_end_captures_nothing() {
    let mut s = Setup::new();
    s.hook(json!({ "hook_event_name": "UserPromptSubmit", "prompt": "fix it" }));
    assert_eq!(s.reasons(), []);
}

/// The ordering, end to end: while the archive is busy with the session's captures, its worktree stays.
#[test]
fn closing_captures_the_conversation_before_the_worktree_is_removed() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    s.close();
    s.wait_held(&hold);
    // Its inspection (on the worktrees' worker) answers meanwhile; the removal still waits for the captures.
    for _ in 0..3 {
        s.h.runtime_mut().run_next(Duration::from_millis(50));
    }
    assert!(s.listed(), "removed before the conversation was archived");
    hold.release();
    s.h.step();
    assert_eq!(
        s.reasons(),
        [CaptureReason::Close, CaptureReason::BeforeRemove]
    );
    assert!(!s.listed(), "removed once archived");
    let closed = s.closed();
    assert!(closed.worktree_removed);
    assert_eq!(closed.worktree_kept, None);
}

#[test]
fn a_capture_that_fails_keeps_the_worktree_and_the_close_says_why() {
    let mut s = Setup::new();
    // The file at the transcript's path is another agent session's: the archive refuses it.
    s.archive.write_transcript(
        transcript(),
        AgentKind::ClaudeCode,
        "11111111-2222-4333-8444-555555555555",
        b"{}\n",
    );
    s.close();
    s.h.step();
    assert!(s.listed(), "kept");
    let closed = s.closed();
    assert!(!closed.worktree_removed);
    let why = closed.worktree_kept.expect("why it was kept");
    assert!(
        why.starts_with("archiving its conversation failed, so it was kept: "),
        "{why}"
    );
    assert!(why.contains(&transcript().display().to_string()), "{why}");
    assert!(s.h.host().sessions().is_empty());
}

/// A transcript the agent never wrote (or deleted since) is nothing to save: the worktree goes, and the close says no
/// transcript was found.
#[test]
fn a_missing_transcript_lets_the_worktree_go_and_the_close_says_so() {
    let mut s = Setup::new();
    s.archive.remove_transcript(&transcript());
    s.close();
    s.h.step();
    assert!(!s.listed(), "removed");
    assert_eq!(s.reasons(), []);
    let closed = s.closed();
    assert!(closed.worktree_removed);
    assert_eq!(closed.worktree_kept, None);
    let note = closed.note.expect("a note");
    assert!(
        note.starts_with("no transcript was found to archive: "),
        "{note}"
    );
}

/// A capture before removal that answers once shutdown has begun removes nothing: the next start finds the session
/// still closing and keeps its worktree.
#[test]
fn a_capture_that_answers_during_shutdown_leaves_the_worktree() {
    let mut s = Setup::new();
    // The archive's calls, in order: the close's capture (put, capture), then the one before removal (put, ...).
    let close_put = s.archive.hold_next();
    s.close();
    s.wait_held(&close_put);
    let close_capture = s.archive.hold_next();
    close_put.release();
    assert!(close_capture.wait_entered(LIMIT), "the close's capture");
    let before_remove = s.archive.hold_next();
    close_capture.release();
    let mut waited = Duration::ZERO;
    while !before_remove.wait_entered(Duration::from_millis(20)) {
        assert!(waited < LIMIT, "the capture before removal never started");
        s.h.runtime_mut().run_next(Duration::from_millis(20));
        waited += Duration::from_millis(40);
    }
    s.h.runtime_mut().handle(RuntimeEvent::Shutdown);
    before_remove.release();
    assert!(s.h.runtime_mut().run_until_settled(LIMIT));
    assert!(s.h.runtime().is_finished());
    assert_eq!(
        s.reasons(),
        [CaptureReason::Close, CaptureReason::BeforeRemove]
    );
    assert!(s.listed(), "removed after shutdown began");
}

/// Captures are bounded: a turn end while another of the session's waits for the archive adds nothing, since the one
/// waiting reads the transcript when it starts.
#[test]
fn turn_ends_waiting_for_the_archive_are_coalesced() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    let stop = s.stop();
    s.send_hook(stop.clone());
    s.wait_held(&hold);
    for _ in 0..3 {
        s.send_hook(stop.clone());
        s.h.poll();
    }
    hold.release();
    s.h.step();
    assert_eq!(
        s.reasons(),
        [CaptureReason::TurnEnd, CaptureReason::TurnEnd],
        "the one running, and one more"
    );
}

/// A client's input still goes through while a capture is held: captures never block the runtime.
#[test]
fn a_capture_in_progress_never_holds_up_input() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    let stop = s.stop();
    s.send_hook(stop);
    s.wait_held(&hold);
    s.client.input(&s.id, b"hello");
    s.h.poll();
    let typed = s.typed.lock().unwrap().clone();
    assert!(typed.ends_with(b"hello"), "{typed:?}");
    hold.release();
    s.h.step();
}

#[test]
fn an_upgrade_while_a_capture_runs_is_busy() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    let stop = s.stop();
    s.send_hook(stop);
    s.wait_held(&hold);
    let req = s
        .client
        .request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    s.h.poll();
    let answer: UpgradeAnswer =
        serde_json::from_value(response(&s.client.take(), &req).unwrap()).unwrap();
    assert!(matches!(answer, UpgradeAnswer::Busy { .. }), "{answer:?}");
    hold.release();
    s.h.step();
}

#[test]
fn shutdown_waits_for_a_capture_in_progress() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    let stop = s.stop();
    s.send_hook(stop);
    s.wait_held(&hold);
    s.h.runtime_mut().handle(RuntimeEvent::Shutdown);
    s.h.poll();
    assert!(
        !s.h.runtime().is_finished(),
        "stopped with a capture running"
    );
    hold.release();
    assert!(s.h.runtime_mut().run_until_settled(LIMIT));
    assert!(s.h.runtime().is_finished());
    assert_eq!(s.reasons(), [CaptureReason::TurnEnd]);
}

#[test]
fn shutdown_waits_for_a_capture_only_so_long() {
    let mut s = Setup::new();
    let hold = s.archive.hold_next();
    let stop = s.stop();
    s.send_hook(stop);
    s.wait_held(&hold);
    s.h.runtime_mut().handle(RuntimeEvent::Shutdown);
    s.h.poll();
    assert!(!s.h.runtime().is_finished());
    // Longer than an ordinary shutdown waits for PTYs: a capture gets as long as a create's cleanup.
    s.h.clock().advance(SHUTDOWN_GRACE);
    s.h.poll();
    assert!(!s.h.runtime().is_finished(), "gave up on the capture early");
    s.h.clock().advance(CREATE_CLEANUP_GRACE - SHUTDOWN_GRACE);
    s.h.poll();
    assert!(s.h.runtime().is_finished(), "past its grace");
    hold.release();
}

#[test]
fn a_shells_worktree_is_removed_with_nothing_archived() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let app = base.join("app");
    let root = base.join("worktrees");
    std::fs::create_dir(&app).unwrap();
    let worktrees = FakeWorktrees::new(&root);
    worktrees.add_repo(&app, "main");
    let archive = FakeArchive::new();
    let mut h = HostHarness::builder()
        .worktrees(worktrees.clone())
        .archive(archive.clone())
        .start();
    let mut client = h.connect();
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: Some(app.to_str().unwrap().to_owned()),
        build: None,
    }));
    h.step();
    let path = root.join("app").join("shell-1");
    std::fs::create_dir_all(&path).unwrap();
    let req = client.request(ClientRequest::SessionNew(SessionNew::default()));
    h.step();
    let id = new_session_id(&client.take(), &req);
    client.request(ClientRequest::SessionClose(SessionClose {
        session: id,
        worktree: CloseWorktree::IfSafe,
    }));
    h.step();
    let listed = worktrees.list_managed(&RepoRef::new(&app)).unwrap();
    assert!(listed.is_empty(), "removed: {listed:?}");
    assert!(archive.snapshots().is_empty());
    assert!(archive.sessions().is_empty());
}
