//! Workspace scenarios (T21): a host runtime on all-fake ports, with the fake `Worktrees` port deciding what is a repo.
//! Paths are real directories in a tempdir, because the host resolves them on the filesystem before asking the port.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use shepherd_core::acpp::{Frame, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{EventLog, LogId};
use shepherd_core::wire::{ClientRequest, Hello, PROTOCOL_VERSION, WireError, WorkspaceSelect};
use shepherd_testkit::fakes::pty::ProgramInput;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A shell that prints the directory it was started in.
fn pwd_shell() -> Program {
    Arc::new(|input, p| {
        if input == ProgramInput::Start {
            let cwd = p.spec().cwd.display().to_string();
            p.print(cwd);
        }
    })
}

/// A tempdir holding `app` (a repo the fake port knows) and `plain` (a directory that isn't a repo), both resolved.
struct Dirs {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    plain: PathBuf,
    root: PathBuf,
}

fn dirs(worktrees: &FakeWorktrees) -> Dirs {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Resolved, as the host stores paths: on macOS the tempdir is behind the `/var` symlink.
    let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
    let app = root.join("app");
    let plain = root.join("plain");
    std::fs::create_dir(&app).expect("app dir");
    std::fs::create_dir(&plain).expect("plain dir");
    worktrees.add_repo(&app, "main");
    Dirs {
        _tmp: tmp,
        app,
        plain,
        root,
    }
}

fn harness(worktrees: &FakeWorktrees) -> HostHarness {
    HostHarness::builder()
        .program(SHELL, pwd_shell())
        .worktrees(worktrees.clone())
        .start()
}

fn hello(client: &mut FakeClient, cwd: Option<&Path>) -> shepherd_core::acpp::RequestId {
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: cwd.map(|c| c.to_str().expect("utf-8 path").to_owned()),
    }))
}

fn select(client: &mut FakeClient, path: &Path) -> shepherd_core::acpp::RequestId {
    client.request(ClientRequest::WorkspaceSelect(WorkspaceSelect {
        path: path.to_str().expect("utf-8 path").to_owned(),
    }))
}

/// The `workspace` and `cwd` session `id`'s `session/created` recorded.
fn created(h: &HostHarness, id: &SessionId) -> (Option<String>, Option<String>) {
    let entries = h
        .log()
        .read(LogId::Session(id.clone()), Seq::ZERO)
        .expect("session log");
    entries
        .iter()
        .find_map(|env| match env.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::SessionCreated(c) => Some((c.workspace.clone(), c.cwd.clone())),
                _ => None,
            },
            _ => None,
        })
        .expect("session/created")
}

#[test]
fn a_client_started_in_a_repo_selects_it_and_new_sessions_start_there_and_record_it() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    let req = hello(&mut client, Some(&d.app));
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));

    client.subscribe_all();
    let req = client.new_session();
    h.step();
    let msgs = client.take();
    let id = new_session_id(&msgs, &req);
    let app = d.app.to_str().unwrap().to_owned();
    assert_eq!(created(&h, &id), (Some(app.clone()), Some(app.clone())));
    assert_eq!(String::from_utf8_lossy(&output(&msgs, &id)), app);
}

#[test]
fn the_selection_is_the_workspace_after_a_restart_and_a_clients_cwd_never_replaces_it() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let other = d.root.join("other");
    std::fs::create_dir(&other).unwrap();
    worktrees.add_repo(&other, "main");
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    hello(&mut client, Some(&d.app));
    h.step();
    h.restart();
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));
    let mut client = h.connect();
    hello(&mut client, Some(&other));
    h.step();
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));
}

#[test]
fn a_client_started_outside_a_repo_selects_nothing() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    let req = hello(&mut client, Some(&d.plain));
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert_eq!(h.host().workspace(), None);
}

#[test]
fn selecting_a_repo_by_a_path_through_a_symlink_and_dot_dot_stores_it_resolved() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let alias = d.root.join("alias");
    std::os::unix::fs::symlink(&d.app, &alias).unwrap();
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    let req = select(&mut client, &alias.join("..").join("alias"));
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));
    assert_eq!(h.replayed().workspaces(), std::slice::from_ref(&d.app));
}

#[test]
fn selecting_a_directory_that_is_not_a_repo_says_why_and_selects_nothing() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    select(&mut client, &d.app);
    h.step();
    for path in [
        d.plain.clone(),
        d.root.join("missing"),
        PathBuf::from("app"),
    ] {
        let req = select(&mut client, &path);
        h.step();
        let result = response(&client.take(), &req);
        assert!(
            matches!(&result, Err(WireError::InvalidParams(why)) if why.contains(path.to_str().unwrap())),
            "{path:?}: {result:?}"
        );
    }
    assert_eq!(h.host().workspaces(), std::slice::from_ref(&d.app));
}

#[test]
fn a_new_session_in_a_workspace_that_is_gone_is_refused_instead_of_starting_elsewhere() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    select(&mut client, &d.app);
    h.step();
    std::fs::remove_dir(&d.app).unwrap();
    let req = client.new_session();
    h.step();
    let result = response(&client.take(), &req);
    assert!(
        matches!(&result, Err(WireError::InvalidParams(why)) if why.contains("workspace")),
        "{result:?}"
    );
    assert!(h.host().sessions().is_empty());
}
