//! What the finder asks of the host (T26): the workspace's orphan worktrees, and a session started in one. A host
//! runtime on all-fake ports, with the fake `Worktrees` port listing on its worker thread. The workspace is a real
//! directory in a tempdir, and a test makes each worktree's directory itself, standing in for git.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use shepherd::host::resolve_worktrees_root;
use shepherd_core::acpp::{Envelope, Frame, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    BaseSpec, EventLog, LogId, RepoRef, WorktreeError, WorktreeName, Worktrees,
};
use shepherd_core::wire::{
    ClientRequest, CloseWorktree, Hello, PROTOCOL_VERSION, SessionClose, SessionNew, WireError,
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

/// A host whose workspace is `<tempdir>/app`, a repo the fake port knows, with a subscribed client. Worktrees go
/// under `<tempdir>/worktrees`.
struct Setup {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    root: PathBuf,
    worktrees: FakeWorktrees,
    h: HostHarness,
    client: FakeClient,
}

impl Setup {
    /// Makes `app`'s worktree `name` on the port and on disk, as git would, with no session in it.
    fn orphan(&self, name: &str) -> PathBuf {
        let name = WorktreeName::new(name).unwrap();
        let created = self
            .worktrees
            .create(&RepoRef::new(&self.app), &name, BaseSpec::OriginDefault)
            .unwrap();
        std::fs::create_dir_all(created.path.as_path()).unwrap();
        created.path.as_path().to_path_buf()
    }

    /// Asks for the orphan worktrees and returns the answer.
    fn list(&mut self) -> Result<serde_json::Value, WireError> {
        let asked = self
            .client
            .request(ClientRequest::WorktreeList(Default::default()));
        self.h.step();
        response(&self.client.take(), &asked)
    }
}

fn setup() -> Setup {
    setup_with(false)
}

/// Like [`setup`], with the worktrees' root configured through a symlink, resolved as the host resolves its own
/// before building the port on it.
fn setup_linked() -> Setup {
    setup_with(true)
}

fn setup_with(linked_root: bool) -> Setup {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let app = base.join("app");
    let root = base.join("worktrees");
    std::fs::create_dir(&app).unwrap();
    if linked_root {
        let real = base.join("worktrees-real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &root).unwrap();
    }
    let worktrees = FakeWorktrees::new(resolve_worktrees_root(&root).unwrap());
    worktrees.add_repo(&app, "main");
    let mut h = HostHarness::builder()
        .program(SHELL, pwd_shell())
        .worktrees(worktrees.clone())
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
    Setup {
        _tmp: tmp,
        app,
        root,
        worktrees,
        h,
        client,
    }
}

/// The `_shepherd/*` frames in session `id`'s log, in order.
fn frames(h: &HostHarness, id: &SessionId) -> Vec<ShepherdFrame> {
    h.log()
        .read(LogId::Session(id.clone()), Seq::ZERO)
        .expect("session log")
        .iter()
        .filter_map(|env: &Envelope| match env.frame() {
            Frame::Shepherd(m) => Some(m.body().clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_orphan_worktrees_are_listed_and_a_session_started_in_one_runs_there_as_its_own() {
    let mut s = setup();
    let old = s.orphan("old-idea");
    // A session in a worktree of its own: not an orphan.
    std::fs::create_dir_all(s.root.join("app").join("shell-1")).unwrap();
    let req = s.client.new_session();
    s.h.step();
    new_session_id(&s.client.take(), &req);
    let old_text = old.to_str().unwrap();
    assert_eq!(
        s.list(),
        Ok(json!({ "worktrees": [{ "path": old_text, "branch": "old-idea" }] }))
    );

    let req = s.client.request(ClientRequest::SessionNew(SessionNew {
        profile: None,
        cwd: Some(old_text.to_owned()),
        branch: Some("old-idea".to_owned()),
    }));
    s.h.step();
    let msgs = s.client.take();
    let id = new_session_id(&msgs, &req);
    // It runs in the worktree, which is recorded as its own: nothing new is made.
    assert_eq!(output(&msgs, &id), old_text.as_bytes());
    let frames = frames(&s.h, &id);
    assert!(
        matches!(&frames[..], [ShepherdFrame::SessionCreated(c)]
            if c.worktree.as_deref() == Some(old_text) && c.branch.as_deref() == Some("old-idea")),
        "{frames:?}"
    );
    assert_eq!(
        s.worktrees
            .list_managed(&RepoRef::new(&s.app))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(s.list(), Ok(json!({ "worktrees": [] })));
    assert_eq!(s.h.replayed(), *s.h.host());
}

#[test]
fn a_worktree_list_the_port_fails_to_give_is_refused_with_the_reason() {
    let mut s = setup();
    s.worktrees
        .fail_next(WorktreeError::Timeout("git worktree list".to_owned()));
    let Err(WireError::InvalidParams(why)) = s.list() else {
        panic!("expected a refusal");
    };
    assert!(why.contains("git worktree list"), "{why}");
}

#[test]
fn under_a_linked_root_a_host_made_worktree_is_no_orphan_and_cant_be_adopted() {
    let mut s = setup_linked();
    std::fs::create_dir_all(s.root.join("app").join("shell-1")).unwrap();
    let req = s.client.new_session();
    s.h.step();
    new_session_id(&s.client.take(), &req);
    assert_eq!(s.list(), Ok(json!({ "worktrees": [] })));
    for spelling in [
        s.root.join("app").join("shell-1"),
        std::fs::canonicalize(s.root.join("app").join("shell-1")).unwrap(),
    ] {
        let Err(WireError::InvalidParams(why)) = start_in(&mut s, &spelling, "shell-1") else {
            panic!("{} was adopted", spelling.display());
        };
        assert!(why.contains("already in use"), "{why}");
    }
}

#[test]
fn under_a_linked_root_closing_an_adopted_session_removes_its_worktree() {
    let mut s = setup_linked();
    let old = s.orphan("old-idea");
    let linked = s.root.join("app").join("old-idea");
    let Ok(answer) = start_in(&mut s, &linked, "old-idea") else {
        panic!("not started");
    };
    let id = SessionId::new(answer["session"].as_str().unwrap());
    s.client.request(ClientRequest::SessionClose(SessionClose {
        session: id.clone(),
        worktree: CloseWorktree::IfSafe,
    }));
    s.h.step();
    let frames = frames(&s.h, &id);
    assert!(
        matches!(frames.last(), Some(ShepherdFrame::SessionClosed(c)) if c.worktree_removed),
        "{frames:?}"
    );
    let managed = s.worktrees.list_managed(&RepoRef::new(&s.app)).unwrap();
    assert!(
        managed.iter().all(|w| w.path.as_path() != old),
        "{managed:?}"
    );
}

/// Asks for a session in worktree `path` on `branch`, and returns the answer.
fn start_in(s: &mut Setup, path: &Path, branch: &str) -> Result<serde_json::Value, WireError> {
    let asked = s.client.request(ClientRequest::SessionNew(SessionNew {
        profile: None,
        cwd: Some(path.to_str().unwrap().to_owned()),
        branch: Some(branch.to_owned()),
    }));
    s.h.step();
    response(&s.client.take(), &asked)
}

#[test]
fn every_spelling_of_a_worktree_in_use_is_the_same_worktree() {
    let mut s = setup_linked();
    let real = s.orphan("old-idea");
    // The same directory through the configured, symlinked root.
    let linked = s.root.join("app").join("old-idea");
    assert_ne!(linked, real);
    assert_eq!(std::fs::canonicalize(&linked).unwrap(), real);
    assert_eq!(
        s.list(),
        Ok(json!({ "worktrees": [{ "path": real.to_str().unwrap(), "branch": "old-idea" }] }))
    );
    // Started through a `..` spelling, it is in use under the symlinked one too.
    let dotted = linked.join("..").join("old-idea");
    assert!(start_in(&mut s, &dotted, "old-idea").is_ok());
    assert_eq!(s.list(), Ok(json!({ "worktrees": [] })));
    let Err(WireError::InvalidParams(why)) = start_in(&mut s, &linked, "old-idea") else {
        panic!("a second session in it was accepted");
    };
    assert!(why.contains("already in use"), "{why}");
}
