//! The client folds its host's screen layout (decision 0022) into its state, resolving `Selected` panes to its own
//! selection.

use shepherd_core::client::Intent;
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, Pane, PaneId};
use shepherd_testkit::builders::ClientDriver;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn default_pane() -> PaneId {
    match Layout::default().current_page().root {
        Pane::Leaf { id, .. } => id,
        Pane::Split { .. } => unreachable!("the default is one pane"),
    }
}

/// What the only pane of the current page shows, as this client resolves it.
fn shown(c: &ClientDriver) -> Content {
    match &c.client().layout().current_page().root {
        Pane::Leaf { content, .. } => content.clone(),
        other => panic!("not one pane: {other:?}"),
    }
}

#[test]
fn before_any_layout_is_recorded_the_client_has_the_default() {
    let c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.client().host_layout(), Layout::default());
    assert_eq!(
        ClientDriver::new().client().host_layout(),
        Layout::default()
    );
}

#[test]
fn the_default_pane_shows_the_selected_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(
        shown(&c),
        Content::session(sid("s2")),
        "the newest is selected"
    );
    c.user(Intent::SelectSession(sid("s1")));
    assert_eq!(shown(&c), Content::session(sid("s1")));
}

#[test]
fn with_nothing_selected_a_selected_pane_shows_nothing() {
    let c = ClientDriver::with_sessions(&[]);
    assert_eq!(c.client().selected(), None);
    assert_eq!(shown(&c), Content::Empty);
}

#[test]
fn each_window_resolves_selected_panes_to_its_own_selection() {
    let mut one = ClientDriver::with_sessions(&["s1", "s2"]);
    let mut two = ClientDriver::with_sessions(&["s1", "s2"]);
    one.user(Intent::SelectSession(sid("s1")));
    two.user(Intent::SelectSession(sid("s2")));
    assert_eq!(shown(&one), Content::session(sid("s1")));
    assert_eq!(shown(&two), Content::session(sid("s2")));
    // Neither window's selection is in what the host recorded.
    assert_eq!(one.client().host_layout(), Layout::default());
}

#[test]
fn a_pinned_session_is_shown_whatever_is_selected() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.host().ui_apply(LayoutOp::Put {
        pane: default_pane(),
        content: Content::session(sid("s1")),
    });
    c.sync();
    c.user(Intent::SelectSession(sid("s2")));
    assert_eq!(shown(&c), Content::session(sid("s1")));
}

#[test]
fn the_client_shows_the_layout_its_host_records() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().ui_apply(LayoutOp::PageNew {
        name: Some("two".into()),
        content: Content::Empty,
    });
    c.sync();
    assert_eq!(c.client().host_layout(), c.host().host().layout());
    assert_eq!(c.client().layout().pages().len(), 2);
}

#[test]
fn a_session_that_closes_leaves_its_pinned_panes_empty_in_the_client_too() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.host().ui_apply(LayoutOp::Put {
        pane: default_pane(),
        content: Content::session(sid("s1")),
    });
    c.sync();
    c.host().close("s1");
    c.sync();
    assert_eq!(shown(&c), Content::Empty);
}
