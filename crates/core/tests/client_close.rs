//! The close confirmation of a session with a worktree says what removing it would lose (T25), through the public
//! `shepherd_core::client` API.

use serde_json::json;
use shepherd_core::client::{ClientCommand, CloseCheck, Intent, Popup};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::Loss;
use shepherd_core::wire::{ClientRequest, CloseWorktree, SessionClose, WireError, WorktreeCheck};
use shepherd_testkit::builders::{ClientDriver, requests};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn bodies(commands: &[ClientCommand]) -> Vec<ClientRequest> {
    requests(commands)
        .into_iter()
        .map(|r| r.body.clone())
        .collect()
}

/// A connected, caught-up client whose host has `/src/app` selected, `s1` running in its worktree and `s2` running without one.
fn with_worktree_session() -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_live_with_bell(false);
    c.host().select_workspace("/src/app");
    c.host().new_session("s1");
    c.host().worktree_ready("s1");
    c.host().new_session_with(
        "s2",
        shepherd_core::host::NewSessionSpec {
            explicit_cwd: true,
            ..shepherd_testkit::builders::shell_session_spec()
        },
    );
    c.sync();
    c
}

fn close(session: &str, worktree: CloseWorktree) -> ClientRequest {
    ClientRequest::SessionClose(SessionClose {
        session: sid(session),
        worktree,
    })
}

/// Opens the close confirmation of `s1` and answers its status request with `answer`.
fn checked(answer: Result<serde_json::Value, WireError>) -> ClientDriver {
    let mut c = with_worktree_session();
    let asked = c.user(Intent::BeginCloseSession(sid("s1")));
    c.answer(&asked, answer);
    c
}

fn status(dirty: bool, ahead: u32, pushed: bool, merged: bool) -> serde_json::Value {
    json!({ "status": { "dirty": dirty, "ahead": ahead, "pushed": pushed, "merged_into_default": merged } })
}

#[test]
fn closing_a_worktree_session_asks_the_host_what_removing_its_worktree_would_lose() {
    let mut c = with_worktree_session();
    let sent = c.user(Intent::BeginCloseSession(sid("s1")));
    assert_eq!(
        bodies(&sent),
        [ClientRequest::WorktreeStatus(WorktreeCheck {
            session: sid("s1")
        })]
    );
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose { session: sid("s1") })
    );
    assert_eq!(c.client().close_check(), Some(&CloseCheck::Checking));
}

#[test]
fn closing_a_session_without_a_worktree_asks_nothing() {
    let mut c = with_worktree_session();
    assert_eq!(c.user(Intent::BeginCloseSession(sid("s2"))), []);
    assert_eq!(c.client().close_check(), None);
    assert_eq!(
        bodies(&c.user(Intent::Confirm)),
        [close("s2", CloseWorktree::IfSafe)]
    );
}

#[test]
fn the_answer_says_whether_removing_the_worktree_loses_anything() {
    let cases = [
        (status(false, 0, false, true), CloseCheck::Safe),
        (status(false, 2, true, false), CloseCheck::Safe),
        (
            status(true, 0, false, true),
            CloseCheck::AtRisk(vec![Loss::UncommittedChanges]),
        ),
        (
            status(true, 2, false, false),
            CloseCheck::AtRisk(vec![
                Loss::UncommittedChanges,
                Loss::Unpushed { commits: 2 },
            ]),
        ),
        (json!({}), CloseCheck::Gone),
    ];
    for (answer, check) in cases {
        let c = checked(Ok(answer.clone()));
        assert_eq!(c.client().close_check(), Some(&check), "{answer}");
    }
}

#[test]
fn a_refused_status_says_why() {
    let c = checked(Err(WireError::InvalidParams(
        "/wt/s1 isn't a worktree shepherd manages".into(),
    )));
    assert_eq!(
        c.client().close_check(),
        Some(&CloseCheck::Failed(
            "/wt/s1 isn't a worktree shepherd manages".into()
        ))
    );
    assert_eq!(c.client().notice(), None);
}

#[test]
fn confirming_a_close_that_loses_nothing_lets_the_host_remove_the_worktree_if_still_safe() {
    for answer in [
        Ok(status(false, 0, false, true)),
        Ok(json!({})),
        Err(WireError::Internal("x".into())),
    ] {
        let mut c = checked(answer);
        assert_eq!(
            bodies(&c.user(Intent::Confirm)),
            [close("s1", CloseWorktree::IfSafe)]
        );
        assert_eq!(c.client().popup(), None);
    }
    // Before the answer, too: the host checks again.
    let mut c = with_worktree_session();
    c.user(Intent::BeginCloseSession(sid("s1")));
    assert_eq!(
        bodies(&c.user(Intent::Confirm)),
        [close("s1", CloseWorktree::IfSafe)]
    );
}

#[test]
fn at_risk_the_buttons_remove_anyway_or_keep_and_confirming_keeps() {
    let cases = [
        (Intent::CloseRemovingWorktree, CloseWorktree::Remove),
        (Intent::CloseKeepingWorktree, CloseWorktree::Keep),
        (Intent::Confirm, CloseWorktree::Keep),
    ];
    for (intent, how) in cases {
        let mut c = checked(Ok(status(true, 0, false, true)));
        assert_eq!(
            bodies(&c.user(intent.clone())),
            [close("s1", how)],
            "{intent:?}"
        );
        assert_eq!(c.client().popup(), None);
        assert_eq!(c.client().close_check(), None);
    }
}

#[test]
fn the_worktree_buttons_do_nothing_without_a_close_confirmation() {
    let mut c = with_worktree_session();
    assert_eq!(c.user(Intent::CloseRemovingWorktree), []);
    assert_eq!(c.user(Intent::CloseKeepingWorktree), []);
}

#[test]
fn an_answer_after_the_confirmation_was_dismissed_changes_nothing() {
    let mut c = with_worktree_session();
    let asked = c.user(Intent::BeginCloseSession(sid("s1")));
    c.user(Intent::Dismiss);
    c.answer(&asked, Ok(status(true, 0, false, true)));
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().close_check(), None);
    assert_eq!(c.client().notice(), None);
    // Nor one for another session's confirmation, opened since.
    let asked = c.user(Intent::BeginCloseSession(sid("s1")));
    c.user(Intent::BeginCloseSession(sid("s2")));
    c.answer(&asked, Ok(status(true, 0, false, true)));
    assert_eq!(c.client().close_check(), None);
}

#[test]
fn a_session_removing_its_worktree_says_so_and_cannot_be_closed_again() {
    let mut c = with_worktree_session();
    c.host()
        .close_with("s1", CloseWorktree::IfSafe, WorkspaceHooks::default());
    c.sync();
    let s1 = c
        .client()
        .sessions()
        .iter()
        .find(|s| s.id == sid("s1"))
        .cloned()
        .unwrap_or_else(|| panic!("s1 is listed until its worktree is dealt with"));
    assert!(s1.removing_worktree);
    assert_eq!(c.user(Intent::BeginCloseSession(sid("s1"))), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn a_worktree_kept_on_close_is_a_notice_once_live() {
    let mut c = with_worktree_session();
    c.host()
        .close_with("s1", CloseWorktree::IfSafe, WorkspaceHooks::default());
    c.host().handle(shepherd_core::host::HostEvent::Restarted);
    c.sync();
    assert_eq!(
        c.client().notice(),
        Some("kept the worktree of shell-1: the host stopped before the worktree was removed")
    );
}

#[test]
fn a_worktree_kept_on_close_in_the_catch_up_is_no_notice() {
    let mut c = ClientDriver::new();
    c.host().select_workspace("/src/app");
    c.host().new_session("s1");
    c.host().worktree_ready("s1");
    c.host()
        .close_with("s1", CloseWorktree::IfSafe, WorkspaceHooks::default());
    c.host().handle(shepherd_core::host::HostEvent::Restarted);
    let subscribe = c.connect();
    assert!(matches!(
        requests(&subscribe)[..],
        [r] if matches!(r.body, ClientRequest::Subscribe(_))
    ));
    c.sync();
    assert_eq!(c.client().notice(), None);
}
