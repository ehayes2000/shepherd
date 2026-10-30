//! A new session in a workspace runs in a worktree of its own (T22): a host runtime on all-fake ports, with the fake
//! `Worktrees` port making the worktree on its worker thread. The workspace is a real directory in a tempdir, because
//! the host resolves it on the filesystem before asking the port; worktree paths are only names to the fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shepherd_core::acpp::{Envelope, Frame, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{EventLog, LogId, RepoRef, WorktreeError, Worktrees};
use shepherd_core::wire::{ClientRequest, Hello, PROTOCOL_VERSION, SessionNew};
use shepherd_testkit::fakes::pty::ProgramInput;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A liveness bound for the held git call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);

/// A shell that prints the directory it was started in.
fn pwd_shell() -> Program {
    Arc::new(|input, p| {
        if input == ProgramInput::Start {
            let cwd = p.spec().cwd.display().to_string();
            p.print(cwd);
        }
    })
}

/// A host whose workspace is `<tempdir>/app`, a repo the fake port knows, with a subscribed client.
struct Setup {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    worktrees: FakeWorktrees,
    h: HostHarness,
    client: FakeClient,
}

fn setup(build: impl FnOnce(HarnessBuilder) -> HarnessBuilder) -> Setup {
    let worktrees = FakeWorktrees::new("/fake/worktrees");
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = std::fs::canonicalize(tmp.path()).unwrap().join("app");
    std::fs::create_dir(&app).unwrap();
    worktrees.add_repo(&app, "main");
    let mut h = build(
        HostHarness::builder()
            .program(SHELL, pwd_shell())
            .worktrees(worktrees.clone()),
    )
    .start();
    let mut client = h.connect();
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: Some(app.to_str().unwrap().to_owned()),
    }));
    h.step();
    assert_eq!(h.host().workspace(), Some(app.as_path()));
    client.subscribe_all();
    h.step();
    client.take();
    Setup {
        _tmp: tmp,
        app,
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
        .filter_map(shepherd_frame)
        .collect()
}

fn shepherd_frame(env: &Envelope) -> Option<ShepherdFrame> {
    match env.frame() {
        Frame::Shepherd(m) => Some(m.body().clone()),
        _ => None,
    }
}

fn methods(frames: &[ShepherdFrame]) -> Vec<&'static str> {
    frames.iter().map(ShepherdFrame::method).collect()
}

#[test]
fn a_new_session_is_listed_while_its_worktree_is_made_then_runs_in_it() {
    let mut s = setup(|b| b);
    let hold = s.worktrees.hold_next();
    let req = s.client.new_session();
    s.h.poll();
    assert!(hold.wait_entered(LIMIT), "create never reached the port");

    // Answered and listed while git is still working, with nothing spawned yet.
    let msgs = s.client.take();
    let id = new_session_id(&msgs, &req);
    let listed = fold(&entries(&msgs)).sessions();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].exited);
    assert!(!s.h.runtime().pty().is_running(&id));

    hold.release();
    s.h.step();
    let path = Path::new("/fake/worktrees/app/shell-1");
    let managed = s.worktrees.list_managed(&RepoRef::new(&s.app)).unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].path.as_path(), path);
    let frames = frames(&s.h, &id);
    assert_eq!(
        methods(&frames),
        ["_shepherd/session/created", "_shepherd/worktree/created"]
    );
    assert!(
        matches!(&frames[1], ShepherdFrame::WorktreeCreated(w)
            if w.path == path.to_str().unwrap() && w.branch == "shell-1" && w.base == "origin/main"),
        "{frames:?}"
    );
    // The shell starts in the worktree.
    assert!(s.h.runtime().pty().is_running(&id));
    let msgs = s.client.take();
    assert_eq!(output(&msgs, &id), path.to_str().unwrap().as_bytes());
    assert_eq!(s.h.replayed(), *s.h.host());
}

#[test]
fn a_failed_fetch_fails_the_session_with_the_reason_and_spawns_nothing() {
    let mut s = setup(|b| b);
    s.worktrees.fail_next(WorktreeError::FetchFailed(
        "could not resolve host".to_owned(),
    ));
    let req = s.client.new_session();
    s.h.step();
    let msgs = s.client.take();
    let id = new_session_id(&msgs, &req);

    let frames = frames(&s.h, &id);
    assert_eq!(
        methods(&frames),
        ["_shepherd/session/created", "_shepherd/session/failed"]
    );
    let listed = fold(&entries(&msgs)).sessions();
    assert_eq!(
        listed[0].failed.as_deref(),
        Some("fetching origin failed: could not resolve host")
    );
    assert!(!s.h.runtime().pty().is_running(&id));
    assert!(output(&msgs, &id).is_empty());
    assert!(
        s.worktrees
            .list_managed(&RepoRef::new(&s.app))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_session_closed_while_its_worktree_is_made_never_spawns() {
    let mut s = setup(|b| b);
    let hold = s.worktrees.hold_next();
    let req = s.client.new_session();
    s.h.poll();
    assert!(hold.wait_entered(LIMIT), "create never reached the port");
    let id = new_session_id(&s.client.take(), &req);
    s.client.request(ClientRequest::SessionClose(
        shepherd_core::wire::SessionClose {
            session: id.clone(),
        },
    ));
    s.h.poll();

    hold.release();
    s.h.step();
    assert!(!s.h.runtime().pty().is_running(&id));
    // Recorded for cleanup, which finds the worktree it left behind through the record.
    assert_eq!(
        methods(&frames(&s.h, &id)),
        [
            "_shepherd/session/created",
            "_shepherd/session/closed",
            "_shepherd/worktree/created"
        ]
    );
    assert_eq!(s.h.runtime().pending_run_files(), 0);
}

#[test]
fn a_session_with_an_explicit_cwd_runs_there_without_a_worktree() {
    let mut s = setup(|b| b);
    let req = s.client.request(ClientRequest::SessionNew(SessionNew {
        profile: None,
        cwd: Some(s.app.to_str().unwrap().to_owned()),
    }));
    s.h.step();
    let msgs = s.client.take();
    let id = new_session_id(&msgs, &req);
    assert_eq!(output(&msgs, &id), s.app.to_str().unwrap().as_bytes());
    assert_eq!(methods(&frames(&s.h, &id)), ["_shepherd/session/created"]);
}

#[test]
fn a_session_still_waiting_for_its_worktree_when_the_host_restarts_is_failed() {
    let mut s = setup(|b| b);
    let hold = s.worktrees.hold_next();
    let req = s.client.new_session();
    s.h.poll();
    assert!(hold.wait_entered(LIMIT), "create never reached the port");
    let id = new_session_id(&s.client.take(), &req);
    // The old host's git call finishes after it is gone; its answer goes nowhere.
    s.h.restart();
    hold.release();
    s.h.step();
    let listed = s.h.host().sessions();
    assert!(listed[0].failed.is_some(), "{listed:?}");
    assert!(!s.h.runtime().pty().is_running(&id));
}

#[test]
fn an_agent_waiting_for_its_worktree_gets_its_run_files_when_it_spawns() {
    // A fake `claude` that records whether its `--settings` file exists when it starts.
    let seen: Arc<Mutex<Vec<bool>>> = Arc::default();
    let record = seen.clone();
    let claude: Program = Arc::new(move |input, p| {
        if input == ProgramInput::Start {
            let args = &p.spec().args;
            let at = args.iter().position(|a| a == "--settings").unwrap();
            record
                .lock()
                .unwrap()
                .push(Path::new(&args[at + 1]).is_file());
        }
    });
    // The built-in `claude` profile runs `claude`.
    let mut s = setup(|b| b.program("claude", claude));
    let hold = s.worktrees.hold_next();
    let req = s.client.request(ClientRequest::SessionNew(SessionNew {
        profile: Some("claude".to_owned()),
        cwd: None,
    }));
    s.h.poll();
    assert!(hold.wait_entered(LIMIT), "create never reached the port");
    new_session_id(&s.client.take(), &req);
    hold.release();
    s.h.step();
    assert_eq!(*seen.lock().unwrap(), [true]);
}
