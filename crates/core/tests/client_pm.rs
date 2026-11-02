//! The project manager's page (T60): `ctrl-b p` (or the pane menu's "Project manager") shows the page with the PM session
//! on the left and the PM panel on the right, starting a PM session (the `pm` profile) when there is none, and finding
//! the existing one by the profile its `session/created` recorded.

use serde_json::json;
use shepherd_core::client::{Cell, ClientCommand, ClientEvent, Intent, PaneMenuItem};
use shepherd_core::config::PM_PROFILE;
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, LayoutOp, Pane, PaneId, Ratio, SplitDir, ViewKind};
use shepherd_core::wire::{ClientRequest, PROTOCOL_VERSION, SessionNew, UiApply, WireError};
use shepherd_testkit::builders::{ClientDriver, HostDriver, pm_session_spec, requests, ts};

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn ui_ops(commands: &[ClientCommand]) -> Vec<LayoutOp> {
    requests(commands)
        .into_iter()
        .filter_map(|r| match &r.body {
            ClientRequest::UiApply(UiApply { op }) => Some(op.clone()),
            _ => None,
        })
        .collect()
}

fn new_sessions(commands: &[ClientCommand]) -> Vec<SessionNew> {
    requests(commands)
        .into_iter()
        .filter_map(|r| match &r.body {
            ClientRequest::SessionNew(new) => Some(new.clone()),
            _ => None,
        })
        .collect()
}

fn new_pm(host: &mut HostDriver, id: &str) {
    host.new_session_with(id, pm_session_spec());
}

/// The page the client asks for to show PM session `pm`.
fn pm_page(pm: &str) -> LayoutOp {
    LayoutOp::PageNewSplit {
        name: Some("pm".to_owned()),
        dir: SplitDir::Horizontal,
        ratio: Ratio::from_permille(600),
        a: Content::session(sid(pm)),
        b: Content::View(ViewKind::PmPanel),
    }
}

/// The leaves of the page with id `page`, with what each shows.
fn leaves_of(c: &ClientDriver, page: usize) -> Vec<(PaneId, Content)> {
    fn walk(pane: &Pane, out: &mut Vec<(PaneId, Content)>) {
        match pane {
            Pane::Leaf { id, content } => out.push((*id, content.clone())),
            Pane::Split { a, b, .. } => {
                walk(a, out);
                walk(b, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&c.client().host_layout().pages()[page].root, &mut out);
    out
}

#[test]
fn a_session_knows_the_profile_it_started_with() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    c.sync();
    let profile = |id: &str| {
        c.client()
            .sessions()
            .iter()
            .find(|s| s.id.as_str() == id)
            .and_then(|s| s.profile.clone())
    };
    assert_eq!(profile("pm1").as_deref(), Some("pm"));
    assert_eq!(profile("s1").as_deref(), Some("shell"));
    assert_eq!(c.client().pm_session(), Some(&sid("pm1")));
}

#[test]
fn opening_the_pm_with_no_pm_session_starts_one_with_the_pm_profile() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.client().pm_session(), None);
    let commands = c.user(Intent::OpenPm);
    let new = new_sessions(&commands);
    assert_eq!(new.len(), 1, "{commands:?}");
    assert_eq!(new[0].profile.as_deref(), Some(PM_PROFILE));
    assert_eq!(
        new[0].cwd, None,
        "an ordinary session, in a worktree of its own"
    );
    assert!(ui_ops(&commands).is_empty(), "no page until it is listed");
}

#[test]
fn once_the_new_pm_is_listed_its_page_is_asked_for() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let commands = c.user(Intent::OpenPm);
    let answered = c.answer(&commands, Ok(json!({ "session": "pm1" })));
    assert!(ui_ops(&answered).is_empty());
    new_pm(c.host(), "pm1");
    let listed = c.sync();
    assert_eq!(ui_ops(&listed), [pm_page("pm1")]);
    // Asked once only.
    c.host().agent_started("pm1", "claude-code", Some("a1"));
    assert!(ui_ops(&c.sync()).is_empty());
}

#[test]
fn the_page_is_asked_for_when_the_pm_is_listed_before_its_answer() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let commands = c.user(Intent::OpenPm);
    new_pm(c.host(), "pm1");
    assert_eq!(ui_ops(&c.sync()), [pm_page("pm1")]);
    let answered = c.answer(&commands, Ok(json!({ "session": "pm1" })));
    assert!(ui_ops(&answered).is_empty());
}

#[test]
fn opening_the_pm_again_while_it_starts_starts_no_other() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::OpenPm);
    let again = c.user(Intent::OpenPm);
    assert!(new_sessions(&again).is_empty(), "{again:?}");
    assert!(ui_ops(&again).is_empty());
}

#[test]
fn a_refused_pm_session_stops_waiting_for_one() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let commands = c.user(Intent::OpenPm);
    c.answer(
        &commands,
        Err(WireError::InvalidParams("unknown profile `pm`".to_owned())),
    );
    assert!(
        c.client().notice().is_some_and(|n| n.contains("pm")),
        "{:?}",
        c.client().notice()
    );
    // A PM started some other way later doesn't pop its page open.
    new_pm(c.host(), "pm1");
    assert!(ui_ops(&c.sync()).is_empty());
    // Opening it now shows that one.
    assert_eq!(ui_ops(&c.user(Intent::OpenPm)), [pm_page("pm1")]);
}

#[test]
fn opening_the_pm_with_a_pm_session_asks_for_its_page_without_starting_another() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    c.host().new_session("s2");
    c.sync();
    let commands = c.user(Intent::OpenPm);
    assert!(new_sessions(&commands).is_empty());
    assert_eq!(ui_ops(&commands), [pm_page("pm1")]);
}

#[test]
fn an_exited_pm_is_shown_to_be_resumed_not_replaced() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    c.host().pty_exited("pm1", Some(0));
    c.sync();
    let commands = c.user(Intent::OpenPm);
    assert!(new_sessions(&commands).is_empty());
    assert_eq!(ui_ops(&commands), [pm_page("pm1")]);
}

#[test]
fn a_running_pm_is_preferred_over_a_newer_exited_one() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    new_pm(c.host(), "pm2");
    c.host().pty_exited("pm2", Some(0));
    c.sync();
    assert_eq!(c.client().pm_session(), Some(&sid("pm1")));
}

/// A client with PM session `pm1` and its page (page two) shown on the host, then page one selected again.
fn with_pm_page() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    c.host().ui_apply(pm_page("pm1"));
    let page_one = c.client().host_layout().pages()[0].id;
    c.sync();
    c.host().ui_apply(LayoutOp::PageSelect { page: page_one });
    c.sync();
    c
}

#[test]
fn opening_the_pm_selects_its_page_and_focuses_the_pm() {
    let mut c = with_pm_page();
    let page = c.client().host_layout().pages()[1].id;
    let commands = c.user(Intent::OpenPm);
    assert!(new_sessions(&commands).is_empty());
    assert_eq!(ui_ops(&commands), [LayoutOp::PageSelect { page }]);
    c.host().ui_apply(LayoutOp::PageSelect { page });
    c.sync();
    let focused = c.client().focused_pane();
    assert_eq!(
        c.client().host_layout().pane(focused),
        Some(&Pane::Leaf {
            id: focused,
            content: Content::session(sid("pm1"))
        })
    );
    // Already shown: nothing to ask.
    assert!(ui_ops(&c.user(Intent::OpenPm)).is_empty());
}

#[test]
fn the_pm_page_whose_pm_closed_gets_a_new_pm_in_its_empty_pane() {
    let mut c = with_pm_page();
    c.host().close("pm1");
    c.sync();
    let page = c.client().host_layout().pages()[1].id;
    let (left, shown) = leaves_of(&c, 1)[0].clone();
    assert_eq!(
        shown,
        Content::Empty,
        "the host empties a closed session's pane"
    );
    assert_eq!(c.client().pm_session(), None);
    let commands = c.user(Intent::OpenPm);
    assert_eq!(new_sessions(&commands).len(), 1);
    assert_eq!(ui_ops(&commands), [LayoutOp::PageSelect { page }]);
    c.host().ui_apply(LayoutOp::PageSelect { page });
    c.sync();
    new_pm(c.host(), "pm2");
    assert_eq!(
        ui_ops(&c.sync()),
        [LayoutOp::Put {
            pane: left,
            content: Content::session(sid("pm2"))
        }]
    );
}

#[test]
fn the_pane_menu_opens_the_project_manager() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    new_pm(c.host(), "pm1");
    c.sync();
    let pane = c.client().focused_pane();
    c.user(Intent::OpenPaneMenu {
        pane,
        at: Cell { col: 3, row: 4 },
    });
    assert_eq!(
        c.client().pane_menu().last(),
        Some(&PaneMenuItem::ProjectManager)
    );
    let commands = c.user(Intent::PaneMenuChoose(PaneMenuItem::ProjectManager));
    assert_eq!(ui_ops(&commands), [pm_page("pm1")]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn with_several_exited_pms_the_newest_is_the_pm() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    for id in ["pm1", "pm2", "pm3"] {
        new_pm(c.host(), id);
        c.host().pty_exited(id, Some(0));
    }
    c.sync();
    assert_eq!(c.client().pm_session(), Some(&sid("pm3")));
    assert_eq!(ui_ops(&c.user(Intent::OpenPm)), [pm_page("pm3")]);
}

#[test]
fn a_pm_start_lost_with_the_connection_is_asked_again_after_reconnecting() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(new_sessions(&c.user(Intent::OpenPm)).len(), 1);
    // The link drops before the host answers: the request went with it.
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    c.answer(
        &hello,
        Ok(json!({ "version": PROTOCOL_VERSION, "machine": "m_test" })),
    );
    let again = c.user(Intent::OpenPm);
    assert_eq!(new_sessions(&again).len(), 1, "{again:?}");
}
