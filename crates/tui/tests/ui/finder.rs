//! The finder, opened from the sidebar's search field and clicked where its results are drawn. Ported from
//! `finder.rs` (T45).

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{ClientRequest, Method};
use shepherd_testkit::builders::ClientDriver;

use crate::harness::Ui;

/// Three sessions in workspace `/src/shepherd`: `fix-login` working, `docs` idle, `refactor` (newest) waiting for its
/// worktree; the finder opened by clicking the search field, and the host's answer listing two orphan worktrees.
fn finder() -> Ui {
    let mut ui = Ui::new(ClientDriver::with_sessions(&[]), 80, 24);
    ui.on_host(|h| {
        h.select_workspace("/src/shepherd");
        for (id, name) in [("s1", "fix-login"), ("s2", "docs"), ("s3", "refactor")] {
            h.new_session(id);
            h.rename(id, name);
        }
        h.worktree_ready("s1");
        h.worktree_ready("s2");
        h.append(
            Some("s2"),
            Source::new(SourceKind::Hook, None),
            json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
        );
    });
    ui.click("⌕ Search").answer(
        Method::WorktreeList,
        Ok(json!({ "worktrees": [
            { "path": "/wt/shepherd/old-idea", "branch": "old-idea" },
            { "path": "/wt/shepherd/spike-7" },
        ] })),
    );
    ui
}

#[test]
fn clicking_a_session_in_the_finder_selects_it() {
    let mut ui = finder();
    // `fix-login` is in the sidebar too.
    ui.click_in(ui.boxed("find"), "fix-login");
    assert_eq!(ui.client().selected(), Some(&SessionId::new("s1")));
    assert_eq!(ui.client().popup(), None);
}

#[test]
fn clicking_an_orphan_worktree_then_start_session_starts_one_in_it() {
    let mut ui = finder();
    // The branch is drawn after the name too.
    ui.click("⎇ old-idea").click("[Start session]");
    let started: Vec<(Option<&str>, Option<&str>)> = ui
        .requests()
        .iter()
        .filter_map(|r| match r {
            ClientRequest::SessionNew(new) => Some((new.cwd.as_deref(), new.branch.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(started, [(Some("/wt/shepherd/old-idea"), Some("old-idea"))]);
    assert_eq!(ui.client().popup(), None);
}

#[test]
fn clicking_a_row_of_the_scrolled_finder_chooses_what_is_drawn_there() {
    let names: Vec<String> = (1..=30).map(|i| format!("name-{i:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut ui = Ui::with_sessions(&names, 80, 12);
    ui.click("⌕ Search")
        .answer(Method::WorktreeList, Ok(json!({ "worktrees": [] })));
    let results = ui.boxed("find");
    // Newest first; three notches down, name-27 is the first row shown.
    ui.scroll_at(results.x + 4, results.y + 2, 3);
    let first = ui.at_in(results, "name-27");
    assert_eq!(first.y, results.y + 1, "the top row:\n{}", ui.screen());
    ui.click_in(results, "name-27");
    assert_eq!(ui.client().selected(), Some(&SessionId::new("s27")));
}
