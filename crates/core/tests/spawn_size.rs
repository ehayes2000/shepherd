//! A session's PTY spawns at the size its client's view last gave it (F42), through the public
//! `shepherd_core::host` API.

use std::path::PathBuf;

use shepherd_core::host::{HostCommand, HostEvent, NewSessionSpec, WorkspaceAsk};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::{PtySize, SpawnSpec};
use shepherd_testkit::builders::{
    HostDriver, claude_resume_spec, claude_session_spec, shell_session_spec, shell_spawn,
};

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize { cols, rows }
}

/// The size of each PTY `commands` spawn.
fn spawned(commands: &[HostCommand]) -> Vec<PtySize> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::SpawnPty { spec, .. } => Some(spec.size),
            _ => None,
        })
        .collect()
}

/// A host with `/src/app` selected as its workspace, so new sessions get worktrees.
fn in_workspace() -> HostDriver {
    let mut host = HostDriver::new();
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/app")),
    });
    host
}

/// A plain session spec whose spawn asks for `cols`x`rows`, as the runtime builds it from a `session/new` with a size.
fn shell_sized(cols: u16, rows: u16) -> NewSessionSpec {
    NewSessionSpec {
        spawn: SpawnSpec {
            size: size(cols, rows),
            ..shell_spawn()
        },
        ..shell_session_spec()
    }
}

// ---------- new sessions ----------

#[test]
fn resize_of_a_worktree_session_before_its_pty_spawns_sizes_the_spawn() {
    let mut host = in_workspace();
    host.new_session("s1");

    assert_eq!(host.resize("s1", 132, 40), Vec::new());

    assert_eq!(spawned(&host.worktree_ready("s1")), vec![size(132, 40)]);
}

#[test]
fn resize_while_post_create_hooks_run_sizes_the_spawn() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.worktree_ready_with_hooks("s1", &["make setup"]);

    host.resize("s1", 120, 30);

    assert_eq!(
        spawned(&host.hook_exited("s1", "make setup", 0)),
        vec![size(120, 30)]
    );
}

#[test]
fn the_latest_resize_before_spawn_wins() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.resize("s1", 100, 30);
    host.resize("s1", 150, 50);

    assert_eq!(spawned(&host.worktree_ready("s1")), vec![size(150, 50)]);
}

#[test]
fn a_plain_session_spawns_at_its_requested_size() {
    let mut host = HostDriver::new();

    assert_eq!(
        spawned(&host.new_session_with("s1", shell_sized(90, 33))),
        vec![size(90, 33)]
    );
}

#[test]
fn resize_after_spawn_still_resizes_the_pty() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.resize("s1", 132, 40);
    host.worktree_ready("s1");

    assert_eq!(
        host.resize("s1", 80, 20),
        vec![HostCommand::ResizePty {
            session: SessionId::new("s1"),
            size: size(80, 20),
        }]
    );
}

#[test]
fn resize_of_an_unknown_session_does_nothing() {
    let mut host = HostDriver::new();

    assert_eq!(host.resize("ghost", 132, 40), Vec::new());
    // A session made later with that id isn't affected by it.
    assert_eq!(
        spawned(&host.new_session_with("ghost", shell_sized(90, 33))),
        vec![size(90, 33)]
    );
}

// ---------- resumes ----------

/// An exited Claude Code session `s1` that can be resumed, started at 80x24.
fn exited_claude() -> HostDriver {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host.agent_started("s1", "claude-code", Some("abc123"));
    host
}

#[test]
fn a_resumed_session_spawns_at_the_last_size_it_had_while_running() {
    let mut host = exited_claude();
    host.resize("s1", 140, 45);
    host.pty_exited("s1", Some(0));

    assert_eq!(
        spawned(&host.resume("s1", claude_resume_spec())),
        vec![size(140, 45)]
    );
}

#[test]
fn resize_of_an_exited_session_sizes_its_resume() {
    let mut host = exited_claude();
    host.pty_exited("s1", Some(0));

    assert_eq!(host.resize("s1", 101, 31), Vec::new());

    assert_eq!(
        spawned(&host.resume("s1", claude_resume_spec())),
        vec![size(101, 31)]
    );
}

#[test]
fn a_resume_with_no_known_size_spawns_at_its_spec_size() {
    let mut host = exited_claude();
    host.pty_exited("s1", Some(0));

    assert_eq!(
        spawned(&host.resume("s1", claude_resume_spec())),
        vec![claude_resume_spec().spawn.size]
    );
}

#[test]
fn an_adopted_session_resumes_at_the_size_it_was_carried_with() {
    let mut host = exited_claude();
    host.adopted("s1", size(160, 48));
    host.pty_exited("s1", Some(0));

    assert_eq!(
        spawned(&host.resume("s1", claude_resume_spec())),
        vec![size(160, 48)]
    );
}
