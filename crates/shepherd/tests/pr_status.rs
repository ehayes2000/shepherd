//! A session's pull request (T27): a host runtime on all-fake ports polls the fake `Forge` on its worker thread from
//! its clock, at most once a minute per session, and appends what changed as `_shepherd/session/pr`, which a
//! subscribed client folds into its session list.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::time::Duration;

use shepherd_core::acpp::{Frame, Pr, PrChecks, PrState, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{BranchName, EventLog, LogId, RepoRef};
use shepherd_core::wire::{ClientRequest, Hello, PROTOCOL_VERSION};
use shepherd_testkit::fakes::forge::{Behavior, FakeForge};
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

const MINUTE: Duration = Duration::from_secs(60);

struct Setup {
    _tmp: tempfile::TempDir,
    forge: FakeForge,
    h: HostHarness,
    client: FakeClient,
    /// The session's id and its worktree.
    session: SessionId,
    worktree: PathBuf,
}

impl Setup {
    /// Moves the clock on by `by` and runs the loop.
    fn wait(&mut self, by: Duration) {
        self.h.clock().advance(by);
        self.h.step();
    }

    fn calls(&self) -> usize {
        self.forge.calls().len()
    }

    /// The pull requests recorded in the session's log, in order.
    fn recorded(&self) -> Vec<Option<Pr>> {
        self.h
            .log()
            .read(LogId::Session(self.session.clone()), Seq::ZERO)
            .unwrap()
            .iter()
            .filter_map(|env| match env.frame() {
                Frame::Shepherd(m) => match m.body() {
                    ShepherdFrame::SessionPr(pr) => Some(pr.pr.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

/// A host whose workspace is a repo the fake `Worktrees` knows, with a subscribed client and one shell session
/// running in its worktree, on branch `shell-1`.
fn setup(forge: FakeForge) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let app = base.join("app");
    let root = base.join("worktrees");
    std::fs::create_dir(&app).unwrap();
    let worktrees = FakeWorktrees::new(&root);
    worktrees.add_repo(&app, "main");
    let mut h = HostHarness::builder()
        .worktrees(worktrees)
        .forge(forge.clone())
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
    let worktree = root.join("app").join("shell-1");
    std::fs::create_dir_all(&worktree).unwrap();
    let req = client.new_session();
    h.step();
    let session = new_session_id(&client.take(), &req);
    assert!(h.runtime().pty().is_running(&session));
    Setup {
        _tmp: tmp,
        forge,
        h,
        client,
        session,
        worktree,
    }
}

fn pr(checks: PrChecks) -> Pr {
    Pr {
        number: 123,
        state: PrState::Open,
        checks,
    }
}

#[test]
fn a_sessions_pr_is_polled_once_a_minute_and_changes_reach_the_client() {
    let forge = FakeForge::new();
    forge.open_pr("shell-1", pr(PrChecks::Pending));
    let mut s = setup(forge);
    assert_eq!(s.calls(), 0, "a brand-new branch isn't asked about at once");

    s.wait(MINUTE - Duration::from_secs(1));
    assert_eq!(s.calls(), 0);
    s.wait(Duration::from_secs(1));
    assert_eq!(
        s.forge.calls(),
        [(RepoRef::new(&s.worktree), BranchName::new("shell-1"))]
    );
    assert_eq!(s.recorded(), [Some(pr(PrChecks::Pending))]);
    // Sent live to the subscribed client.
    let live: Vec<_> = entries(&s.client.take());
    assert!(
        live.iter().any(|env| matches!(env.frame(),
            Frame::Shepherd(m) if matches!(m.body(), ShepherdFrame::SessionPr(_)))),
        "{live:?}"
    );
    // A client attaching now lists the session with its PR.
    let mut late = s.h.connect();
    late.subscribe_all();
    s.h.step();
    let listed = fold(&entries(&late.take())).sessions();
    assert_eq!(listed[0].pr, Some(pr(PrChecks::Pending)));

    // No more than once a minute, and the same answer records nothing.
    s.wait(MINUTE / 2);
    assert_eq!(s.calls(), 1);
    s.wait(MINUTE / 2);
    assert_eq!(s.calls(), 2);
    assert_eq!(s.recorded().len(), 1);

    s.forge.update_pr(123, pr(PrChecks::Passing));
    s.wait(MINUTE);
    assert_eq!(s.calls(), 3);
    assert_eq!(
        s.recorded(),
        [Some(pr(PrChecks::Pending)), Some(pr(PrChecks::Passing))]
    );
    assert_eq!(s.h.replayed(), *s.h.host());
}

#[test]
fn a_missing_gh_records_nothing_and_backs_off() {
    let forge = FakeForge::new();
    forge.set_behavior(Behavior::Missing);
    let mut s = setup(forge);
    s.wait(MINUTE);
    assert_eq!(s.calls(), 1);
    assert_eq!(s.recorded(), []);
    assert_eq!(s.h.host().sessions()[0].pr, None);
    // The next call waits two minutes, not one.
    s.wait(MINUTE);
    assert_eq!(s.calls(), 1);
    s.wait(MINUTE);
    assert_eq!(s.calls(), 2);
}

#[test]
fn a_restarted_host_checks_at_once_and_records_only_a_change() {
    let forge = FakeForge::new();
    forge.open_pr("shell-1", pr(PrChecks::Pending));
    let mut s = setup(forge);
    s.wait(MINUTE);
    assert_eq!(s.recorded().len(), 1);
    s.h.restart();
    assert_eq!(s.calls(), 2, "the restarted host asks at once");
    assert_eq!(s.recorded().len(), 1, "the same PR isn't recorded again");
}

#[test]
fn a_closed_session_is_no_longer_polled() {
    let mut s = setup(FakeForge::new());
    s.client.request(ClientRequest::SessionClose(
        shepherd_core::wire::SessionClose {
            session: s.session.clone(),
            worktree: shepherd_core::wire::CloseWorktree::Keep,
        },
    ));
    s.h.step();
    s.wait(MINUTE * 10);
    assert_eq!(s.calls(), 0);
}

#[test]
fn a_restart_that_records_nothing_still_checks_overdue_sessions_at_once() {
    let mut s = setup(FakeForge::new());
    let session = s.session.clone();
    s.client.input(&session, b"exit\r");
    s.h.step();
    assert!(!s.h.runtime().pty().is_running(&session));
    s.wait(MINUTE);
    assert_eq!(s.calls(), 1);
    let entries = |h: &HostHarness| -> usize {
        let log = h.log();
        log.logs()
            .unwrap()
            .into_iter()
            .map(|id| log.read(id, Seq::ZERO).unwrap().len())
            .sum()
    };
    let before = entries(&s.h);
    s.h.restart();
    // Its PTY had already exited, so the new host appended nothing at start: no append scheduled the check.
    assert_eq!(entries(&s.h), before, "the restart appended something");
    assert_eq!(s.calls(), 2, "the restarted host asks at once");
}
