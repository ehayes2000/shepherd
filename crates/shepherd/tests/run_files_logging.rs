//! Closing a session with no run directory logs nothing; a run directory that can't be removed is logged. Alone in its
//! own test binary: tracing caches each callsite's interest process-wide, so another test hitting the same callsite
//! with no subscriber, at the same moment, could make this test's thread-local subscriber miss it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::os::unix::fs::PermissionsExt;

use shepherd::host;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{ClientRequest, SessionClose, SessionNew};
use support::{FakeClient, HostHarness, echo_shell, response};

const WARNING: &str = "cannot remove run files";

fn new_session(h: &mut HostHarness, client: &mut FakeClient, profile: &str) -> SessionId {
    let sent = client.request(ClientRequest::SessionNew(SessionNew {
        profile: Some(profile.to_owned()),
        cwd: None,
        branch: None,
        size: None,
        name: None,
    }));
    h.step();
    let result = response(&client.take(), &sent).unwrap();
    SessionId::new(result["session"].as_str().unwrap())
}

fn close(h: &mut HostHarness, client: &mut FakeClient, session: &SessionId) {
    let sent = client.request(ClientRequest::SessionClose(SessionClose {
        session: session.clone(),
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
    }));
    h.step();
    response(&client.take(), &sent).unwrap();
}

#[test]
fn only_a_run_directory_that_cannot_be_removed_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let file = host::open_log_file(&root).unwrap();
    let mut h = HostHarness::builder()
        .program("claude", echo_shell())
        .start();
    let mut client = h.connect();

    let (shell, claude) = tracing::subscriber::with_default(host::file_subscriber(file), || {
        // No run directory: nothing to remove, nothing to say.
        let shell = new_session(&mut h, &mut client, "shell");
        close(&mut h, &mut client, &shell);

        // A run root nothing can be removed from: worth a warning.
        let claude = new_session(&mut h, &mut client, "claude");
        let run_root = h.run_root().to_path_buf();
        std::fs::set_permissions(&run_root, std::fs::Permissions::from_mode(0o500)).unwrap();
        close(&mut h, &mut client, &claude);
        std::fs::set_permissions(&run_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        (shell, claude)
    });

    let logged = std::fs::read_to_string(root.join(host::LOG_FILE)).unwrap();
    let warnings: Vec<&str> = logged.lines().filter(|l| l.contains(WARNING)).collect();
    assert_eq!(warnings.len(), 1, "{logged}");
    assert!(warnings[0].contains(claude.as_str()), "{logged}");
    assert!(!logged.contains(shell.as_str()), "{logged}");
}
