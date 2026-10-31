//! The client folds its host's screen layout (decision 0022) into its state.

use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, Pane};
use shepherd_testkit::builders::ClientDriver;

fn put_first(c: &mut ClientDriver, session: &str) {
    let Pane::Leaf { id, .. } = Layout::default().current_page().root else {
        panic!("the default is one pane");
    };
    c.host().ui_apply(LayoutOp::Put {
        pane: id,
        content: Content::Session(SessionId::new(session)),
    });
}

#[test]
fn before_any_layout_is_recorded_the_client_has_the_default() {
    let c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.client().layout(), Layout::default());
    assert_eq!(ClientDriver::new().client().layout(), Layout::default());
}

#[test]
fn the_client_shows_the_layout_its_host_records() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    put_first(&mut c, "s1");
    c.host().ui_apply(LayoutOp::PageNew {
        name: Some("two".into()),
        content: Content::Empty,
    });
    c.sync();
    assert_eq!(c.client().layout(), c.host().host().layout());
    assert_eq!(c.client().layout().pages().len(), 2);
}

#[test]
fn a_session_that_closes_leaves_its_panes_empty_in_the_client_too() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    put_first(&mut c, "s1");
    c.sync();
    c.host().close("s1");
    c.sync();
    assert_eq!(c.client().layout(), Layout::default());
}
