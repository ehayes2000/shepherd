//! An agent session whose cwd is in no git repo can't be archived (there is no repo to file it under, T42b): the host
//! says so once for that session, not on every turn end, and nothing is held up. Alone in its own test binary:
//! tracing caches each callsite's interest process-wide, so another test hitting the same callsite with no subscriber,
//! at the same moment, could make this test's thread-local subscriber miss it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use shepherd::host;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{ClientRequest, CloseWorktree, HookFrame, SessionClose, SessionNew};
use shepherd_testkit::fakes::archive::FakeArchive;
use support::{CLAUDE_HOME, FakeClient, HostHarness, echo_shell, new_session_id, response};

const OUTSIDE: &str = "can't be archived";
const FAILED: &str = "archiving a conversation failed";
const AGENT_SESSION: &str = "0a1b2c3d-4e5f-4061-8728-394a5b6c7d8e";

fn hook(
    h: &mut HostHarness,
    client: &mut FakeClient,
    session: &SessionId,
    payload: serde_json::Value,
) {
    let sent = client.request(ClientRequest::HookFrame(Box::new(HookFrame {
        session: session.clone(),
        agent: AgentKind::ClaudeCode,
        payload,
    })));
    h.step();
    response(&client.take(), &sent).unwrap();
}

#[test]
fn an_agent_outside_any_repo_is_said_to_be_unarchivable_once_and_holds_nothing_up() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let file = host::open_log_file(&root).unwrap();
    // A plain directory: the harness's fake `Worktrees` knows no repo, so it is in none.
    let dir = std::fs::canonicalize(tmp.path()).unwrap().join("notes");
    std::fs::create_dir(&dir).unwrap();
    let archive = FakeArchive::new();
    let mut h = HostHarness::builder()
        .archive(archive.clone())
        .program("claude", echo_shell())
        .start();
    let mut client = h.connect();
    let transcript = format!("{CLAUDE_HOME}/projects/-notes/{AGENT_SESSION}.jsonl");
    archive.write_transcript(&transcript, AgentKind::ClaudeCode, AGENT_SESSION, b"{}\n");

    let session = tracing::subscriber::with_default(host::file_subscriber(file), || {
        let sent = client.request(ClientRequest::SessionNew(SessionNew {
            profile: Some("claude".to_owned()),
            cwd: Some(dir.to_str().unwrap().to_owned()),
            ..SessionNew::default()
        }));
        h.step();
        let session = new_session_id(&client.take(), &sent);
        let start = json!({
            "hook_event_name": "SessionStart",
            "session_id": AGENT_SESSION,
            "transcript_path": transcript,
            "source": "startup",
        });
        hook(&mut h, &mut client, &session, start);
        for _ in 0..3 {
            let stop = json!({ "hook_event_name": "Stop", "session_id": AGENT_SESSION });
            hook(&mut h, &mut client, &session, stop);
        }
        // Closing it waits on nothing: it has no worktree, and its close's capture is skipped the same way.
        let sent = client.request(ClientRequest::SessionClose(SessionClose {
            session: session.clone(),
            worktree: CloseWorktree::IfSafe,
        }));
        h.step();
        response(&client.take(), &sent).unwrap();
        session
    });

    assert!(h.host().sessions().is_empty(), "closed");
    assert!(archive.snapshots().is_empty());
    let logged = std::fs::read_to_string(root.join(host::LOG_FILE)).unwrap();
    let said: Vec<&str> = logged.lines().filter(|l| l.contains(OUTSIDE)).collect();
    assert_eq!(said.len(), 1, "{logged}");
    assert!(said[0].contains(session.as_str()), "{logged}");
    assert!(said[0].contains(dir.to_str().unwrap()), "{logged}");
    assert!(!logged.contains(FAILED), "{logged}");
}
