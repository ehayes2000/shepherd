//! Workspace scenarios (T21): a host runtime on all-fake ports, with the fake `Worktrees` port deciding what is a repo.
//! Paths are real directories in a tempdir, because the host resolves them on the filesystem before asking the port.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use shepherd_core::acpp::{Envelope, Frame, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{EventLog, LogId};
use shepherd_core::wire::{
    ClientRequest, Hello, PROTOCOL_VERSION, SessionRename, WireError, WorkspaceSelect,
};
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
fn a_client_started_in_a_repo_selects_it_and_new_sessions_start_in_a_worktree_of_it_and_record_it()
{
    // The fake only records worktrees; its directory is made here, as git would, so the shell can start in it.
    let wt = tempfile::tempdir().unwrap();
    let wt_root = std::fs::canonicalize(wt.path()).unwrap();
    let shell_1 = wt_root.join("app").join("shell-1");
    std::fs::create_dir_all(&shell_1).unwrap();
    let worktrees = FakeWorktrees::new(&wt_root);
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
    // Its cwd is its worktree (T22), recorded in `worktree/created` rather than `session/created`.
    assert_eq!(created(&h, &id), (Some(app), None));
    assert_eq!(
        String::from_utf8_lossy(&output(&msgs, &id)),
        shell_1.to_str().unwrap()
    );
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
    let file = d.root.join("file");
    std::fs::write(&file, b"not a dir").unwrap();
    // Each refusal names the path and says why.
    for (path, why) in [
        (d.plain.clone(), "not a git repository"),
        (d.root.join("missing"), "No such file"),
        (PathBuf::from("app"), "not an absolute path"),
        (file, "not a directory"),
    ] {
        let req = select(&mut client, &path);
        h.step();
        let result = response(&client.take(), &req);
        assert!(
            matches!(&result, Err(WireError::InvalidParams(msg))
                if msg.contains(path.to_str().unwrap()) && msg.contains(why)),
            "{path:?}: {result:?}"
        );
    }
    assert_eq!(h.host().workspaces(), std::slice::from_ref(&d.app));
}

#[test]
fn a_hello_the_host_refuses_selects_nothing_from_its_cwd() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    let req = client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION + 1,
        machine: MachineId::new("laptop"),
        cwd: Some(d.app.to_str().unwrap().to_owned()),
    }));
    h.step();
    assert!(matches!(
        response(&client.take(), &req),
        Err(WireError::VersionMismatch { .. })
    ));
    assert_eq!(h.host().workspace(), None);
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

#[test]
fn while_a_workspace_probe_is_stuck_in_git_other_sessions_and_clients_are_still_served() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    // The default shell echoes what it is typed.
    let mut h = HostHarness::builder().worktrees(worktrees.clone()).start();
    let mut watcher = h.connect();
    watcher.subscribe_all();
    let req = watcher.new_session();
    h.step();
    let session = new_session_id(&watcher.take(), &req);

    let hold = worktrees.hold_next();
    let mut selector = h.connect();
    let selecting = select(&mut selector, &d.app);
    h.poll();
    assert!(
        hold.wait_entered(Duration::from_secs(10)),
        "the probe never reached the port"
    );

    // Counted, not timed: the session's output and a request's answer both arrive while git is still stuck.
    watcher.input(&session, b"still here");
    let rename = watcher.request(ClientRequest::SessionRename(SessionRename {
        session: session.clone(),
        name: "busy".to_owned(),
    }));
    h.poll();
    let msgs = watcher.take();
    assert_eq!(output(&msgs, &session), b"still here");
    assert!(response(&msgs, &rename).is_ok());
    assert!(
        selector.take().is_empty(),
        "the select was answered before its probe finished"
    );
    assert_eq!(h.host().workspace(), None);

    hold.release();
    h.step();
    assert!(response(&selector.take(), &selecting).is_ok());
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));
}

/// The `workspace/selected` entries among `envelopes`, as the repos they selected.
fn selections(envelopes: &[Envelope]) -> Vec<String> {
    envelopes
        .iter()
        .filter_map(|env| match env.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::WorkspaceSelected(s) => Some(s.repo.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn host_selections(h: &HostHarness) -> Vec<String> {
    selections(&h.log().read(LogId::Host, Seq::ZERO).expect("host log"))
}

/// `app/crates`, a real directory inside the fake repo `app`.
fn subdir(d: &Dirs) -> PathBuf {
    let sub = d.app.join("crates");
    std::fs::create_dir(&sub).unwrap();
    sub
}

#[test]
fn a_client_started_in_a_subdirectory_of_a_repo_selects_the_repo() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let sub = subdir(&d);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    hello(&mut client, Some(&sub));
    h.step();
    assert_eq!(h.host().workspace(), Some(d.app.as_path()));
}

#[test]
fn selecting_a_subdirectory_of_the_workspace_is_the_same_workspace() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let sub = subdir(&d);
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    select(&mut client, &d.app);
    h.step();
    let req = select(&mut client, &sub);
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert_eq!(h.host().workspaces(), std::slice::from_ref(&d.app));
    let app = d.app.to_str().unwrap().to_owned();
    assert_eq!(host_selections(&h), vec![app], "selected again");
}

#[test]
fn a_typed_path_starting_with_a_tilde_is_in_the_hosts_home() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    // The harness's home is a real directory; a fake repo in its `src` (which exists) stands in for `~/Code/x`.
    let home = std::fs::canonicalize(HOME).unwrap();
    let repo = home.join("src");
    worktrees.add_repo(&repo, "main");
    let mut h = harness(&worktrees);
    let mut client = h.connect();
    let req = select(&mut client, Path::new("~/src"));
    h.step();
    assert!(response(&client.take(), &req).is_ok());
    assert_eq!(h.host().workspace(), Some(repo.as_path()));

    // `~` alone is the home itself, which the fake doesn't know as a repo. `~name` and other relative paths are refused.
    for (typed, why) in [
        ("~", "not a git repository"),
        ("~root/src", "not an absolute path"),
        ("src", "not an absolute path"),
    ] {
        let req = select(&mut client, Path::new(typed));
        h.step();
        let result = response(&client.take(), &req);
        assert!(
            matches!(&result, Err(WireError::InvalidParams(msg)) if msg.contains(why)),
            "{typed}: {result:?}"
        );
    }
    assert_eq!(h.host().workspaces(), std::slice::from_ref(&repo));
}

/// The probe of a hello's cwd outlasts the client's subscribe, so the selection reaches it live, after its catch-up.
#[test]
fn a_selection_from_a_hellos_cwd_that_ends_after_the_subscribe_arrives_live() {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let d = dirs(&worktrees);
    let mut h = harness(&worktrees);
    let hold = worktrees.hold_next();
    let mut client = h.connect();
    hello(&mut client, Some(&d.app));
    h.poll();
    assert!(
        hold.wait_entered(Duration::from_secs(10)),
        "the probe never reached the port"
    );
    let sub = client.subscribe_all();
    h.poll();
    let msgs = client.take();
    assert!(response(&msgs, &sub).is_ok(), "the catch-up should be over");
    assert!(selections(&entries(&msgs)).is_empty(), "{msgs:?}");

    hold.release();
    h.step();
    let app = d.app.to_str().unwrap().to_owned();
    assert_eq!(selections(&entries(&client.take())), vec![app]);
}
