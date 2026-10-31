//! The cleanup view (T25b): orphan worktrees and stale sessions with what removing each would lose, and bulk removal of
//! the safe ones, one at a time. Through the public `shepherd_core::client` API.

use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::client::{
    CleanupCheck, CleanupItem, CleanupListing, CleanupOutcome, ClientCommand, ClientEvent, Intent,
    Orphan, Popup,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::WorktreeStatus;
use shepherd_core::wire::{
    ClientRequest, CloseWorktree, SessionClose, WireError, WorktreeCheck, WorktreeCleanup,
    WorktreeRemove,
};
use shepherd_testkit::builders::{ClientDriver, requests, ts};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn bodies(commands: &[ClientCommand]) -> Vec<ClientRequest> {
    requests(commands)
        .into_iter()
        .map(|r| r.body.clone())
        .collect()
}

fn status(dirty: bool, ahead: u32, merged: bool) -> Value {
    json!({ "dirty": dirty, "ahead": ahead, "pushed": false, "merged_into_default": merged })
}

fn clean_merged() -> WorktreeStatus {
    WorktreeStatus {
        dirty: false,
        ahead: 0,
        pushed: false,
        merged_into_default: true,
    }
}

/// A connected client in workspace `/src/app`, the time a tick says being `2026-10-29T12:00:00Z`.
fn in_workspace() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/app");
    c.sync();
    c.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:00Z"),
    });
    c
}

/// Opens the cleanup view, and answers its list with `worktrees` and 7 days. Returns what the answer sent.
fn open(c: &mut ClientDriver, worktrees: Value) -> Vec<ClientCommand> {
    let asked = c.user(Intent::BeginCleanup);
    assert_eq!(
        bodies(&asked),
        [ClientRequest::WorktreeCleanup(WorktreeCleanup::default())]
    );
    c.answer(
        &asked,
        Ok(json!({ "worktrees": worktrees, "cleanup_after_days": 7 })),
    )
}

fn two_orphans() -> Value {
    json!([
        { "path": "/wt/app/old-idea", "branch": "old-idea", "status": status(false, 0, true) },
        { "path": "/wt/app/wip", "branch": "wip", "status": status(true, 2, false) },
    ])
}

fn old_idea() -> CleanupItem {
    CleanupItem::Orphan(Orphan {
        path: PathBuf::from("/wt/app/old-idea"),
        branch: Some("old-idea".into()),
    })
}

fn items(c: &ClientDriver) -> Vec<CleanupItem> {
    c.client()
        .cleanup_rows()
        .iter()
        .map(|r| r.item.clone())
        .collect()
}

fn outcomes(c: &ClientDriver) -> Vec<Option<CleanupOutcome>> {
    c.client()
        .cleanup_rows()
        .iter()
        .map(|r| r.outcome.clone())
        .collect()
}

/// Session `id` with a worktree in the workspace, its PTY exited at `at`.
fn exited_at(c: &mut ClientDriver, id: &str, at: &str) {
    c.host().new_session(id);
    c.host().worktree_ready(id);
    c.host().pin_ts(at);
    c.host().pty_exited(id, Some(0));
    c.host().unpin_ts();
    c.sync();
}

fn answer_status(c: &mut ClientDriver, asked: &[ClientCommand], status: Value) {
    c.answer(asked, Ok(json!({ "status": status })));
}

// ---------- opening ----------

#[test]
fn the_cleanup_button_opens_the_view_and_lists_the_orphans_with_their_status() {
    let mut c = in_workspace();
    let asked = c.user(Intent::BeginCleanup);
    assert_eq!(c.client().popup(), Some(&Popup::Cleanup));
    assert_eq!(c.client().cleanup_listing(), Some(&CleanupListing::Loading));
    c.answer(
        &asked,
        Ok(json!({ "worktrees": [
            { "path": "/wt/app/old-idea", "branch": "old-idea", "status": status(false, 0, true) },
            { "path": "/wt/app/broken", "error": "git broke" },
        ], "cleanup_after_days": 7 })),
    );
    assert_eq!(c.client().cleanup_listing(), Some(&CleanupListing::Listed));
    let rows = c.client().cleanup_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].item, old_idea());
    assert_eq!(rows[0].check, CleanupCheck::Checked(clean_merged()));
    assert!(rows[0].safe());
    assert_eq!(rows[1].check, CleanupCheck::Failed("git broke".into()));
    assert!(!rows[1].safe());
}

#[test]
fn a_list_the_host_refuses_says_why() {
    let mut c = in_workspace();
    let asked = c.user(Intent::BeginCleanup);
    c.answer(&asked, Err(WireError::InvalidParams("not a repo".into())));
    assert_eq!(
        c.client().cleanup_listing(),
        Some(&CleanupListing::Failed("not a repo".into()))
    );
}

#[test]
fn only_dirty_or_unmerged_and_unpushed_worktrees_are_unsafe() {
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    let safe: Vec<bool> = c.client().cleanup_rows().iter().map(|r| r.safe()).collect();
    assert_eq!(safe, [true, false]);
    assert_eq!(c.client().cleanup_safe(), 1);
}

#[test]
fn a_session_exited_long_enough_ago_is_checked_and_listed_only_if_merged() {
    let mut c = in_workspace();
    // s1 exited 28 days before the tick, s2 3 days before, s3 is running.
    exited_at(&mut c, "s1", "2026-10-01T12:00:00Z");
    exited_at(&mut c, "s2", "2026-10-26T12:00:00Z");
    c.host().new_session("s3");
    c.host().worktree_ready("s3");
    c.sync();
    let asked = open(&mut c, json!([]));
    assert_eq!(
        bodies(&asked),
        [ClientRequest::WorktreeStatus(WorktreeCheck {
            session: sid("s1")
        })]
    );
    assert_eq!(
        c.client().cleanup_rows()[0].check,
        CleanupCheck::Checking,
        "listed while it is checked"
    );
    answer_status(&mut c, &asked, status(false, 0, true));
    assert!(matches!(
        &items(&c)[..],
        [CleanupItem::Session { id, branch, .. }] if *id == sid("s1") && branch.as_deref() == Some("s1")
    ));
    assert!(c.client().cleanup_rows()[0].safe());
}

#[test]
fn an_exited_session_whose_branch_isnt_merged_is_not_stale() {
    let mut c = in_workspace();
    exited_at(&mut c, "s1", "2026-10-01T12:00:00Z");
    let asked = open(&mut c, json!([]));
    answer_status(&mut c, &asked, status(false, 1, false));
    assert_eq!(items(&c), []);
}

#[test]
fn the_days_come_from_the_hosts_answer() {
    let mut c = in_workspace();
    exited_at(&mut c, "s1", "2026-10-26T12:00:00Z");
    let asked = c.user(Intent::BeginCleanup);
    let checks = c.answer(
        &asked,
        Ok(json!({ "worktrees": [], "cleanup_after_days": 2 })),
    );
    assert_eq!(bodies(&checks).len(), 1, "3 days is stale after 2");
}

#[test]
fn without_a_workspace_there_is_nothing_to_list_but_the_view_opens() {
    let mut c = ClientDriver::with_sessions(&[]);
    let asked = c.user(Intent::BeginCleanup);
    assert_eq!(c.client().popup(), Some(&Popup::Cleanup));
    assert_eq!(bodies(&asked).len(), 1);
}

#[test]
fn opening_while_disconnected_says_so() {
    let mut c = in_workspace();
    c.handle(ClientEvent::Disconnected);
    let asked = c.user(Intent::BeginCleanup);
    assert!(bodies(&asked).is_empty());
    assert!(matches!(
        c.client().cleanup_listing(),
        Some(CleanupListing::Failed(_))
    ));
}

// ---------- removing ----------

#[test]
fn removing_the_safe_ones_removes_them_one_at_a_time_and_never_an_unsafe_one() {
    let mut c = in_workspace();
    exited_at(&mut c, "s1", "2026-10-01T12:00:00Z");
    let asked = open(&mut c, two_orphans());
    answer_status(&mut c, &asked, status(false, 0, true));
    // old-idea (safe), wip (unsafe), s1 (safe).
    let first = c.user(Intent::CleanupRemoveSafe);
    assert_eq!(
        bodies(&first),
        [ClientRequest::WorktreeRemove(WorktreeRemove {
            path: "/wt/app/old-idea".into(),
        })]
    );
    assert_eq!(
        outcomes(&c),
        [
            Some(CleanupOutcome::Removing),
            None,
            Some(CleanupOutcome::Queued)
        ]
    );
    let second = c.answer(&first, Ok(json!({ "removed": true })));
    assert_eq!(
        bodies(&second),
        [ClientRequest::SessionClose(SessionClose {
            session: sid("s1"),
            worktree: CloseWorktree::IfSafe,
        })]
    );
    // The close is acked at once; its outcome is the session's `session/closed`.
    assert_eq!(c.answer(&second, Ok(json!({}))), []);
    assert_eq!(outcomes(&c)[2], Some(CleanupOutcome::Removing));
    c.host().close("s1");
    c.host()
        .handle(shepherd_core::host::HostEvent::WorktreeRemoved {
            remover: shepherd_core::host::Remover::Session(sid("s1")),
            path: shepherd_core::ports::WorktreePath::new("/wt/s1"),
            result: Ok(()),
            exists: false,
            note: None,
        });
    c.sync();
    assert_eq!(
        outcomes(&c),
        [
            Some(CleanupOutcome::Removed),
            None,
            Some(CleanupOutcome::Removed)
        ]
    );
    assert_eq!(c.client().cleanup_safe(), 0);
}

#[test]
fn a_worktree_the_host_kept_says_why() {
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    let first = c.user(Intent::CleanupRemoveSafe);
    c.answer(
        &first,
        Ok(json!({ "removed": false, "kept": "it has uncommitted changes" })),
    );
    assert_eq!(
        outcomes(&c)[0],
        Some(CleanupOutcome::Kept("it has uncommitted changes".into()))
    );
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    let first = c.user(Intent::CleanupRemoveSafe);
    c.answer(&first, Err(WireError::InvalidParams("in use by s9".into())));
    assert_eq!(
        outcomes(&c)[0],
        Some(CleanupOutcome::Kept("in use by s9".into()))
    );
    assert_eq!(c.client().notice(), None, "the row says it");
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    let first = c.user(Intent::CleanupRemoveSafe);
    c.answer(&first, Ok(json!({ "removed": false })));
    assert_eq!(outcomes(&c)[0], Some(CleanupOutcome::Gone));
}

#[test]
fn closing_the_view_doesnt_stop_the_removals_and_a_notice_says_how_they_went() {
    let mut c = in_workspace();
    let both = json!([
        { "path": "/wt/app/a", "branch": "a", "status": status(false, 0, true) },
        { "path": "/wt/app/b", "branch": "b", "status": status(false, 0, true) },
    ]);
    open(&mut c, both);
    let first = c.user(Intent::CleanupRemoveSafe);
    c.user(Intent::Dismiss);
    assert_eq!(c.client().popup(), None);
    let second = c.answer(&first, Ok(json!({ "removed": true })));
    assert_eq!(bodies(&second).len(), 1, "the next one still goes");
    c.answer(&second, Ok(json!({ "removed": false, "kept": "nope" })));
    assert_eq!(c.client().notice(), Some("cleanup: removed 1, kept 1"));
    // Reopened while it ran, the view shows the rows as they are; reopened after, it lists afresh.
    let again = c.user(Intent::BeginCleanup);
    assert_eq!(bodies(&again).len(), 1);
}

#[test]
fn reopening_the_view_while_removals_run_shows_them_without_asking_again() {
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    c.user(Intent::CleanupRemoveSafe);
    c.user(Intent::Dismiss);
    let reopened = c.user(Intent::BeginCleanup);
    assert!(bodies(&reopened).is_empty());
    assert_eq!(outcomes(&c)[0], Some(CleanupOutcome::Removing));
}

#[test]
fn a_dropped_connection_leaves_the_removal_under_way_unknown_and_unqueues_the_rest() {
    let mut c = in_workspace();
    let both = json!([
        { "path": "/wt/app/a", "branch": "a", "status": status(false, 0, true) },
        { "path": "/wt/app/b", "branch": "b", "status": status(false, 0, true) },
    ]);
    open(&mut c, both);
    c.user(Intent::CleanupRemoveSafe);
    c.handle(ClientEvent::Disconnected);
    // The host most likely finishes it: it isn't shown, or counted, as kept.
    assert_eq!(outcomes(&c), [Some(CleanupOutcome::Unknown), None]);
}

/// A stale session's check answered while a removal runs brings `[Remove safe]` back; a second click queues it behind
/// the removal under way instead of starting a second one.
#[test]
fn removing_again_while_a_removal_runs_queues_behind_it() {
    let mut c = in_workspace();
    exited_at(&mut c, "s1", "2026-10-01T12:00:00Z");
    let check = open(&mut c, two_orphans());
    let first = c.user(Intent::CleanupRemoveSafe);
    assert_eq!(bodies(&first).len(), 1, "old-idea");
    answer_status(&mut c, &check, status(false, 0, true));
    assert_eq!(c.client().cleanup_safe(), 1, "s1 is safe now");
    assert_eq!(c.user(Intent::CleanupRemoveSafe), [], "one at a time");
    assert_eq!(outcomes(&c)[2], Some(CleanupOutcome::Queued));
    let next = c.answer(&first, Ok(json!({ "removed": true })));
    assert_eq!(
        bodies(&next),
        [ClientRequest::SessionClose(SessionClose {
            session: sid("s1"),
            worktree: CloseWorktree::IfSafe,
        })]
    );
}

#[test]
fn a_session_from_another_workspace_is_never_stale_here() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/other");
    exited_at(&mut c, "s1", "2026-10-01T12:00:00Z");
    c.host().select_workspace("/src/app");
    c.sync();
    c.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:00Z"),
    });
    let checks = open(&mut c, json!([]));
    assert_eq!(bodies(&checks), []);
    assert_eq!(items(&c), []);
}

#[test]
fn removing_needs_the_view_open() {
    let mut c = in_workspace();
    open(&mut c, two_orphans());
    c.user(Intent::Dismiss);
    assert!(c.user(Intent::CleanupRemoveSafe).is_empty());
}

#[test]
fn the_list_scrolls_within_its_rows() {
    let mut c = in_workspace();
    let many: Vec<Value> = (0..20)
        .map(|i| json!({ "path": format!("/wt/app/w{i}"), "status": status(false, 0, true) }))
        .collect();
    open(&mut c, Value::Array(many));
    c.user(Intent::ScrollCleanup { by: 3, shown: 5 });
    assert_eq!(c.client().cleanup_scroll(), 3);
    c.user(Intent::ScrollCleanup { by: 100, shown: 5 });
    assert_eq!(c.client().cleanup_scroll(), 15);
    c.user(Intent::ScrollCleanup { by: -100, shown: 5 });
    assert_eq!(c.client().cleanup_scroll(), 0);
}
