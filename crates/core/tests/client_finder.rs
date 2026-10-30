//! The finder (T26): search sessions and the workspace's orphan worktrees, jump to a session, or start one in a
//! worktree. Through the public `shepherd_core::client` API.

use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::client::{ClientCommand, FinderItem, Focus, Intent, Orphan, Orphans, Popup};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{ClientRequest, SessionNew, ViewSize, WireError};
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

fn session(id: &str) -> FinderItem {
    FinderItem::Session(sid(id))
}

fn orphan(path: &str, branch: Option<&str>) -> Orphan {
    Orphan {
        path: PathBuf::from(path),
        branch: branch.map(str::to_owned),
    }
}

fn worktree(path: &str, branch: Option<&str>) -> FinderItem {
    FinderItem::Worktree(orphan(path, branch))
}

/// Sessions `ids` (the last newest), each renamed to the name beside it.
fn named(sessions: &[(&str, &str)]) -> ClientDriver {
    let ids: Vec<&str> = sessions.iter().map(|(id, _)| *id).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    for (id, name) in sessions {
        c.host().rename(id, name);
    }
    c.sync();
    c
}

/// Opens the finder and answers its worktree list with `worktrees` (path, branch).
fn find(c: &mut ClientDriver, worktrees: Value) -> Vec<ClientCommand> {
    let asked = c.user(Intent::BeginFind);
    c.answer(&asked, Ok(json!({ "worktrees": worktrees })))
}

fn results(c: &ClientDriver) -> Vec<FinderItem> {
    c.client().finder()
}

#[test]
fn clicking_the_search_field_opens_the_finder_and_asks_the_host_for_its_worktrees() {
    let mut c = named(&[("s1", "docs")]);
    let sent = c.user(Intent::BeginFind);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Finder {
            query: String::new()
        })
    );
    assert_eq!(
        bodies(&sent),
        [ClientRequest::WorktreeList(Default::default())]
    );
    assert_eq!(c.client().orphans(), Some(&Orphans::Loading));
}

#[test]
fn an_empty_query_lists_every_session_newest_first_then_the_orphan_worktrees() {
    let mut c = named(&[("s1", "docs"), ("s2", "api")]);
    find(
        &mut c,
        json!([{ "path": "/wt/app/old-idea", "branch": "old-idea" }, { "path": "/wt/app/detached" }]),
    );
    assert_eq!(
        results(&c),
        [
            session("s2"),
            session("s1"),
            worktree("/wt/app/old-idea", Some("old-idea")),
            worktree("/wt/app/detached", None),
        ]
    );
    assert_eq!(
        c.client().orphans(),
        Some(&Orphans::Listed(vec![
            orphan("/wt/app/old-idea", Some("old-idea")),
            orphan("/wt/app/detached", None),
        ]))
    );
}

#[test]
fn typing_filters_the_results_and_ranks_prefixes_then_word_starts_then_subsequences() {
    // Newest first is blog, fix-login, login-page: the ranking reorders them.
    let mut c = named(&[
        ("s1", "login-page"),
        ("s2", "fix-login"),
        ("s3", "blog"),
        ("s4", "docs"),
    ]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind("log".into()));
    assert_eq!(results(&c), [session("s1"), session("s2"), session("s3")]);
}

#[test]
fn sessions_match_by_their_branch_workspace_and_machine() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/api");
    c.host().new_session("s1");
    c.host().rename("s1", "one");
    c.host().worktree_ready("s1");
    c.host().select_workspace("/src/web");
    c.host().new_session("s2");
    c.host().rename("s2", "two");
    c.sync();
    find(&mut c, json!([]));
    // worktree_ready puts s1 on branch `s1`.
    c.user(Intent::EditFind("s1".into()));
    assert_eq!(results(&c), [session("s1")], "branch");
    c.user(Intent::EditFind("web".into()));
    assert_eq!(results(&c), [session("s2")], "workspace");
    // ClientDriver's host is machine `m_test`.
    c.user(Intent::EditFind("m_test".into()));
    assert_eq!(results(&c), [session("s2"), session("s1")], "machine");
}

#[test]
fn orphan_worktrees_match_by_their_branch_and_directory_name() {
    let mut c = named(&[("s1", "docs")]);
    find(
        &mut c,
        json!([{ "path": "/wt/app/spike", "branch": "try-sqlite" }, { "path": "/wt/app/detached" }]),
    );
    c.user(Intent::EditFind("sqlite".into()));
    assert_eq!(results(&c), [worktree("/wt/app/spike", Some("try-sqlite"))]);
    c.user(Intent::EditFind("detached".into()));
    assert_eq!(results(&c), [worktree("/wt/app/detached", None)]);
}

#[test]
fn the_query_is_trimmed() {
    let mut c = named(&[("s1", "docs"), ("s2", "api")]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind(" docs ".into()));
    assert_eq!(results(&c), [session("s1")]);
}

#[test]
fn a_query_nothing_matches_leaves_no_results() {
    let mut c = named(&[("s1", "docs")]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind("zzz".into()));
    assert_eq!(results(&c), []);
}

#[test]
fn choosing_a_session_selects_it_closes_the_finder_and_gives_it_the_keys() {
    let mut c = named(&[("s1", "docs"), ("s2", "api")]);
    find(&mut c, json!([]));
    c.user(Intent::SetFocus(Focus::Command));
    c.user(Intent::FinderChoose(session("s1")));
    assert_eq!(c.client().selected(), Some(&sid("s1")));
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().focus(), Focus::Terminal);
}

#[test]
fn enter_chooses_the_first_result() {
    let mut c = named(&[("s1", "docs"), ("s2", "api")]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind("do".into()));
    c.user(Intent::Confirm);
    assert_eq!(c.client().selected(), Some(&sid("s1")));
    assert_eq!(c.client().popup(), None);
}

#[test]
fn enter_with_no_results_leaves_the_finder_open() {
    let mut c = named(&[("s1", "docs")]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind("zzz".into()));
    let sent = c.user(Intent::Confirm);
    assert_eq!(sent, []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Finder {
            query: "zzz".into()
        })
    );
}

#[test]
fn choosing_something_that_isnt_a_result_does_nothing() {
    let mut c = named(&[("s1", "docs"), ("s2", "api")]);
    find(&mut c, json!([]));
    c.user(Intent::EditFind("api".into()));
    c.user(Intent::FinderChoose(session("s1")));
    assert_eq!(c.client().selected(), Some(&sid("s2")));
    assert!(matches!(c.client().popup(), Some(Popup::Finder { .. })));
}

#[test]
fn choosing_an_orphan_worktree_offers_to_start_a_session_in_it() {
    let mut c = named(&[("s1", "docs")]);
    find(
        &mut c,
        json!([{ "path": "/wt/app/old-idea", "branch": "old-idea" }]),
    );
    let sent = c.user(Intent::FinderChoose(worktree(
        "/wt/app/old-idea",
        Some("old-idea"),
    )));
    assert_eq!(sent, []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::StartInWorktree {
            worktree: orphan("/wt/app/old-idea", Some("old-idea"))
        })
    );
}

#[test]
fn starting_a_session_in_a_worktree_asks_for_one_there_on_its_branch() {
    let mut c = named(&[("s1", "docs")]);
    find(
        &mut c,
        json!([{ "path": "/wt/app/old-idea", "branch": "old-idea" }]),
    );
    c.user(Intent::EditFind("old".into()));
    c.user(Intent::Confirm);
    let sent = c.user(Intent::Confirm);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionNew(SessionNew {
            profile: None,
            cwd: Some("/wt/app/old-idea".into()),
            branch: Some("old-idea".into()),
            size: None,
        })]
    );
    assert_eq!(c.client().popup(), None);
}

#[test]
fn starting_a_session_in_a_worktree_asks_to_spawn_at_the_views_size() {
    let mut c = named(&[("s1", "docs")]);
    c.user(Intent::Resize(PtySize {
        cols: 120,
        rows: 40,
    }));
    find(
        &mut c,
        json!([{ "path": "/wt/app/old-idea", "branch": "old-idea" }]),
    );
    c.user(Intent::EditFind("old".into()));
    c.user(Intent::Confirm);
    let sent = c.user(Intent::Confirm);
    assert!(
        matches!(
            &bodies(&sent)[..],
            [ClientRequest::SessionNew(SessionNew {
                size: Some(ViewSize {
                    cols: 120,
                    rows: 40
                }),
                ..
            })]
        ),
        "{sent:?}"
    );
}

#[test]
fn a_host_that_cant_list_its_worktrees_says_why_in_the_finder() {
    let mut c = named(&[("s1", "docs")]);
    let asked = c.user(Intent::BeginFind);
    c.answer(
        &asked,
        Err(WireError::InvalidParams("git timed out".into())),
    );
    assert_eq!(
        c.client().orphans(),
        Some(&Orphans::Failed("git timed out".into()))
    );
    assert_eq!(c.client().notice(), None);
    assert_eq!(results(&c), [session("s1")], "sessions are still found");
}

#[test]
fn a_host_older_than_the_finder_is_said_to_be_older() {
    let mut c = named(&[("s1", "docs")]);
    let asked = c.user(Intent::BeginFind);
    c.answer(
        &asked,
        Err(WireError::MethodNotFound("_shepherd/worktree/list".into())),
    );
    let Some(Orphans::Failed(why)) = c.client().orphans() else {
        panic!("{:?}", c.client().orphans());
    };
    assert!(why.contains("older"), "{why}");
}

#[test]
fn an_answer_after_the_finder_closed_is_ignored_and_reopening_asks_again() {
    let mut c = named(&[("s1", "docs")]);
    let asked = c.user(Intent::BeginFind);
    c.user(Intent::Dismiss);
    c.answer(
        &asked,
        Ok(json!({ "worktrees": [{ "path": "/wt/app/x", "branch": "x" }] })),
    );
    assert_eq!(c.client().orphans(), None);
    let again = c.user(Intent::BeginFind);
    assert_eq!(bodies(&again).len(), 1);
    assert_eq!(c.client().orphans(), Some(&Orphans::Loading));
    // The first answer, arriving late, isn't taken for the second.
    c.answer(
        &asked,
        Ok(json!({ "worktrees": [{ "path": "/wt/app/x", "branch": "x" }] })),
    );
    assert_eq!(c.client().orphans(), Some(&Orphans::Loading));
}

#[test]
fn the_first_finders_answer_arriving_after_it_was_reopened_is_not_taken_for_the_second() {
    let mut c = named(&[("s1", "docs")]);
    let first = c.user(Intent::BeginFind);
    c.user(Intent::Dismiss);
    let second = c.user(Intent::BeginFind);
    c.answer(
        &first,
        Ok(json!({ "worktrees": [{ "path": "/wt/app/x", "branch": "x" }] })),
    );
    assert_eq!(c.client().orphans(), Some(&Orphans::Loading));
    c.answer(&second, Ok(json!({ "worktrees": [] })));
    assert_eq!(c.client().orphans(), Some(&Orphans::Listed(vec![])));
}

#[test]
fn the_finder_opened_while_disconnected_says_so_instead_of_asking() {
    let mut c = ClientDriver::new();
    let sent = c.user(Intent::BeginFind);
    assert_eq!(sent, []);
    assert!(matches!(c.client().orphans(), Some(Orphans::Failed(_))));
}

#[test]
fn the_link_dropping_while_the_worktrees_load_says_so_instead_of_loading_forever() {
    let mut c = named(&[("s1", "docs")]);
    c.user(Intent::BeginFind);
    c.handle(shepherd_core::client::ClientEvent::Disconnected);
    assert!(matches!(c.client().orphans(), Some(Orphans::Failed(_))));
}

#[test]
fn scrolling_the_finder_stays_within_its_results_and_typing_scrolls_back_to_the_top() {
    let ids: Vec<String> = (1..=10).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    find(&mut c, json!([]));
    c.user(Intent::ScrollFinder { by: 3, shown: 4 });
    assert_eq!(c.client().finder_scroll(), 3);
    c.user(Intent::ScrollFinder { by: 100, shown: 4 });
    assert_eq!(c.client().finder_scroll(), 6);
    c.user(Intent::ScrollFinder { by: -100, shown: 4 });
    assert_eq!(c.client().finder_scroll(), 0);
    c.user(Intent::ScrollFinder { by: 5, shown: 4 });
    c.user(Intent::EditFind("s".into()));
    assert_eq!(c.client().finder_scroll(), 0);
}
