//! The status page's table (T57): every session grouped by what the user owes it or by machine, the header's counts,
//! the table's own selection and scroll, attaching a session, and opening the page.

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::status::{Group, Grouping, STATUS_PAGE, StatusCounts, StatusLine};
use shepherd_core::client::{ClientCommand, Focus, Intent};
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, LayoutOp, Pane, PaneId, SplitDir, ViewKind};
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{ClientNotification, ClientRequest, SessionResize, UiApply};
use shepherd_testkit::builders::{ClientDriver, notifications, requests};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn frame(c: &mut ClientDriver, session: &str, frame: serde_json::Value) {
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), hook, frame);
    c.sync();
}

fn idle(c: &mut ClientDriver, session: &str) {
    let idle = json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} });
    frame(c, session, idle);
}

fn asks(c: &mut ClientDriver, session: &str) {
    let ask = json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
        "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": "Bash(rm)" }, "options": [] } });
    frame(c, session, ask);
}

/// `s1` idle, `s2` exited, `s3` working (starting), `s4` asking permission, `s5` idle (newest, selected).
fn mixed() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3", "s4", "s5"]);
    idle(&mut c, "s1");
    c.host().pty_exited("s2", Some(0));
    c.sync();
    asks(&mut c, "s4");
    idle(&mut c, "s5");
    c
}

/// The table as text: `#group n`, `@machine n`, a session's id, or `-` for a gap.
fn table(c: &ClientDriver) -> Vec<String> {
    c.client()
        .status_lines()
        .iter()
        .map(|l| match l {
            StatusLine::Group { group, count } => format!("#{group:?} {count}"),
            StatusLine::Machine { machine, count } => format!("@{} {count}", machine.name),
            StatusLine::Session(s) => s.id.as_str().to_owned(),
            StatusLine::Gap => "-".to_owned(),
        })
        .collect()
}

fn only_pane(c: &ClientDriver) -> PaneId {
    match &c.client().host_layout().current_page().root {
        Pane::Leaf { id, .. } => *id,
        other => panic!("not one pane: {other:?}"),
    }
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

#[test]
fn by_state_the_table_lists_needs_you_working_idle_exited_each_under_its_header() {
    let c = mixed();
    assert_eq!(c.client().status_grouping(), Grouping::State);
    assert_eq!(
        table(&c),
        [
            "#NeedsYou 1",
            "s4",
            "-",
            "#Working 1",
            "s3",
            "-",
            "#Idle 2",
            "s5",
            "s1",
            "-",
            "#Exited 1",
            "s2"
        ]
    );
}

#[test]
fn a_group_without_sessions_has_no_header() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    idle(&mut c, "s1");
    assert_eq!(table(&c), ["#Working 1", "s2", "-", "#Idle 1", "s1"]);
}

#[test]
fn a_session_whose_subagent_waits_on_the_user_needs_the_user() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    idle(&mut c, "s1");
    let started = json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/started",
        "params": { "id": "a1", "agent_type": "builder" } });
    frame(&mut c, "s1", started);
    let blocked = json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/blocked",
        "params": { "id": "a1", "kind": "permission", "text": "Bash(ls)" } });
    frame(&mut c, "s1", blocked);
    let session = &c.client().sessions()[0];
    assert_eq!(Group::of(c.client(), session), Group::NeedsYou);
}

#[test]
fn the_header_counts_each_group_the_subagents_and_the_machines() {
    let mut c = mixed();
    let started = json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/started",
        "params": { "id": "a1", "agent_type": "builder" } });
    frame(&mut c, "s3", started);
    assert_eq!(
        c.client().status_counts(),
        StatusCounts {
            needs_you: 1,
            working: 1,
            idle: 2,
            exited: 1,
            subagents: 1,
            machines: 1,
            online: 1,
        }
    );
}

#[test]
fn by_machine_each_machine_lists_its_sessions_by_state() {
    let mut c = mixed();
    c.user(Intent::StatusGroupBy {
        by: Grouping::Machine,
        shown: 10,
    });
    assert_eq!(c.client().status_grouping(), Grouping::Machine);
    assert_eq!(table(&c), ["@local 5", "s4", "s3", "s5", "s1", "s2"]);
    c.user(Intent::StatusGroupBy {
        by: Grouping::State,
        shown: 10,
    });
    assert_eq!(table(&c)[0], "#NeedsYou 1");
}

#[test]
fn the_tables_selection_starts_at_its_first_row_and_is_not_the_sidebars() {
    let mut c = mixed();
    assert_eq!(c.client().status_selected(), Some(&sid("s4")));
    c.user(Intent::StatusSelect {
        session: sid("s1"),
        shown: 10,
    });
    assert_eq!(c.client().status_selected(), Some(&sid("s1")));
    assert_eq!(
        c.client().selected(),
        Some(&sid("s5")),
        "the sidebar keeps its own"
    );
    c.user(Intent::StatusSelect {
        session: sid("nope"),
        shown: 10,
    });
    assert_eq!(c.client().status_selected(), Some(&sid("s1")));
}

#[test]
fn a_selected_session_that_closes_gives_way_to_the_first_row() {
    let mut c = mixed();
    c.user(Intent::StatusSelect {
        session: sid("s1"),
        shown: 10,
    });
    c.host().close("s1");
    c.sync();
    assert_eq!(c.client().status_selected(), Some(&sid("s4")));
}

#[test]
fn stepping_moves_through_the_sessions_skipping_headers_and_stops_at_the_ends() {
    let mut c = mixed();
    let step = |c: &mut ClientDriver, by| {
        c.user(Intent::StatusStep { by, shown: 20 });
        c.client().status_selected().cloned()
    };
    assert_eq!(step(&mut c, 1), Some(sid("s3")));
    assert_eq!(step(&mut c, 1), Some(sid("s5")));
    assert_eq!(step(&mut c, 10), Some(sid("s2")));
    assert_eq!(step(&mut c, 1), Some(sid("s2")));
    assert_eq!(step(&mut c, -100), Some(sid("s4")));
}

#[test]
fn stepping_scrolls_the_selection_into_view() {
    // 12 lines; 3 shown at once.
    let mut c = mixed();
    assert_eq!(c.client().status_scroll(), 0);
    c.user(Intent::StatusStep { by: 4, shown: 3 });
    assert_eq!(c.client().status_selected(), Some(&sid("s2")));
    assert_eq!(c.client().status_scroll(), 9, "s2 is the last line, 11");
    c.user(Intent::StatusStep { by: -2, shown: 3 });
    assert_eq!(c.client().status_selected(), Some(&sid("s5")));
    assert_eq!(
        c.client().status_scroll(),
        7,
        "s5 is line 7, the first shown"
    );
}

#[test]
fn the_wheel_scrolls_the_table_and_leaves_the_selection() {
    let mut c = mixed();
    c.user(Intent::StatusScroll { by: 5, shown: 3 });
    assert_eq!(c.client().status_scroll(), 5);
    assert_eq!(c.client().status_selected(), Some(&sid("s4")));
    c.user(Intent::StatusScroll { by: 50, shown: 3 });
    assert_eq!(
        c.client().status_scroll(),
        9,
        "no further than the last lines fill it"
    );
    c.user(Intent::StatusScroll { by: -50, shown: 3 });
    assert_eq!(c.client().status_scroll(), 0);
}

#[test]
fn regrouping_keeps_the_selection_in_view() {
    let mut c = mixed();
    c.user(Intent::StatusSelect {
        session: sid("s2"),
        shown: 20,
    });
    c.user(Intent::StatusGroupBy {
        by: Grouping::Machine,
        shown: 2,
    });
    // By machine, s2 (exited) is the last of six lines.
    assert_eq!(c.client().status_scroll(), 4);
    assert_eq!(c.client().status_selected(), Some(&sid("s2")));
}

#[test]
fn attaching_selects_the_session_and_focuses_the_pane_showing_the_selection() {
    let mut c = mixed();
    c.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    let status = only_pane(&c);
    // The status page on the left, the selection on the right.
    c.host().ui_apply(LayoutOp::Put {
        pane: status,
        content: Content::View(ViewKind::Status),
    });
    c.host().ui_apply(LayoutOp::Split {
        pane: status,
        dir: SplitDir::Horizontal,
        ratio: Default::default(),
        content: Content::Selected,
    });
    c.sync();
    c.user(Intent::FocusPane(status));
    c.user(Intent::SetFocus(Focus::Command));
    let commands = c.user(Intent::StatusAttach(sid("s1")));
    assert_eq!(c.client().selected(), Some(&sid("s1")));
    assert_eq!(c.client().status_selected(), Some(&sid("s1")));
    assert_ne!(c.client().focused_pane(), status);
    assert_eq!(c.client().focused_session(), Some(sid("s1")));
    assert_eq!(c.client().focus(), Focus::Terminal);
    let resized: Vec<&SessionId> = notifications(&commands)
        .into_iter()
        .filter_map(|n| match n {
            ClientNotification::SessionResize(SessionResize { session, .. }) => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(resized, [&sid("s1")], "sized to its pane, once");
    assert!(ui_ops(&commands).is_empty(), "the page stays");
}

#[test]
fn attaching_from_a_page_without_one_selects_the_page_that_has_one() {
    let mut c = mixed();
    let first = only_pane(&c);
    c.host().ui_apply(LayoutOp::PageNew {
        name: Some("status".to_owned()),
        content: Content::View(ViewKind::Status),
    });
    c.sync();
    let page_one = c.client().host_layout().pages()[0].id;
    let commands = c.user(Intent::StatusAttach(sid("s3")));
    assert_eq!(ui_ops(&commands), [LayoutOp::PageSelect { page: page_one }]);
    assert_eq!(c.client().selected(), Some(&sid("s3")));
    c.host().ui_apply(LayoutOp::PageSelect { page: page_one });
    c.sync();
    assert_eq!(c.client().focused_pane(), first);
    assert_eq!(c.client().focused_session(), Some(sid("s3")));
}

#[test]
fn attaching_with_no_pane_to_show_it_asks_for_a_page_showing_the_selection() {
    let mut c = mixed();
    let pane = only_pane(&c);
    c.host().ui_apply(LayoutOp::Put {
        pane,
        content: Content::View(ViewKind::Status),
    });
    c.sync();
    let commands = c.user(Intent::StatusAttach(sid("s3")));
    assert_eq!(
        ui_ops(&commands),
        [LayoutOp::PageNew {
            name: None,
            content: Content::Selected,
        }]
    );
    assert_eq!(c.client().selected(), Some(&sid("s3")));
}

#[test]
fn attaching_a_session_that_isnt_listed_does_nothing() {
    let mut c = mixed();
    let commands = c.user(Intent::StatusAttach(sid("nope")));
    assert!(commands.is_empty());
    assert_eq!(c.client().selected(), Some(&sid("s5")));
}

#[test]
fn opening_the_status_page_asks_for_one_when_there_is_none() {
    let mut c = mixed();
    let commands = c.user(Intent::OpenStatus);
    assert_eq!(
        ui_ops(&commands),
        [LayoutOp::PageNew {
            name: Some(STATUS_PAGE.to_owned()),
            content: Content::View(ViewKind::Status),
        }]
    );
}

#[test]
fn opening_the_status_page_selects_its_page_and_focuses_it() {
    let mut c = mixed();
    c.host().ui_apply(LayoutOp::PageNew {
        name: Some("status".to_owned()),
        content: Content::View(ViewKind::Status),
    });
    let page_one = c.client().host_layout().pages()[0].id;
    c.host().ui_apply(LayoutOp::PageSelect { page: page_one });
    c.sync();
    let status_page = c.client().host_layout().pages()[1].id;
    let commands = c.user(Intent::OpenStatus);
    assert_eq!(
        ui_ops(&commands),
        [LayoutOp::PageSelect { page: status_page }]
    );
    c.host()
        .ui_apply(LayoutOp::PageSelect { page: status_page });
    c.sync();
    let focused = c.client().focused_pane();
    assert!(matches!(
        c.client().host_layout().pane(focused),
        Some(Pane::Leaf {
            content: Content::View(ViewKind::Status),
            ..
        })
    ));
    // Already shown: nothing to ask.
    assert!(ui_ops(&c.user(Intent::OpenStatus)).is_empty());
}

#[test]
fn opening_the_status_page_focuses_its_pane_on_the_current_page() {
    let mut c = mixed();
    c.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    let first = only_pane(&c);
    c.host().ui_apply(LayoutOp::Split {
        pane: first,
        dir: SplitDir::Horizontal,
        ratio: Default::default(),
        content: Content::View(ViewKind::Status),
    });
    c.sync();
    assert_eq!(c.client().focused_pane(), first);
    let commands = c.user(Intent::OpenStatus);
    assert!(ui_ops(&commands).is_empty());
    let focused = c.client().focused_pane();
    assert_ne!(focused, first);
    assert!(matches!(
        c.client().host_layout().pane(focused),
        Some(Pane::Leaf {
            content: Content::View(ViewKind::Status),
            ..
        })
    ));
}

#[test]
fn selecting_a_row_out_of_view_scrolls_it_into_view() {
    // 12 lines; 3 shown at once; s2 is the last line, 11.
    let mut c = mixed();
    c.user(Intent::StatusSelect {
        session: sid("s2"),
        shown: 3,
    });
    assert_eq!(c.client().status_scroll(), 9);
}

/// Each session resized by `commands`.
fn resized(commands: &[ClientCommand]) -> Vec<SessionId> {
    notifications(commands)
        .into_iter()
        .filter_map(|n| match n {
            ClientNotification::SessionResize(SessionResize { session, .. }) => {
                Some(session.clone())
            }
            _ => None,
        })
        .collect()
}

/// `mixed`, sized, with the status page current: page one is its only pane showing `first`, page two (`status`) the
/// status view.
fn status_on_page_two(first: Content) -> ClientDriver {
    let mut c = mixed();
    c.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    let pane = only_pane(&c);
    c.host().ui_apply(LayoutOp::Put {
        pane,
        content: first,
    });
    c.host().ui_apply(LayoutOp::PageNew {
        name: Some("status".to_owned()),
        content: Content::View(ViewKind::Status),
    });
    c.sync();
    c
}

#[test]
fn attaching_selects_the_page_of_a_pane_pinning_the_session() {
    let mut c = status_on_page_two(Content::session(sid("s1")));
    let page_one = c.client().host_layout().pages()[0].id;
    let pinned = match &c.client().host_layout().pages()[0].root {
        Pane::Leaf { id, .. } => *id,
        other => panic!("not one pane: {other:?}"),
    };
    let commands = c.user(Intent::StatusAttach(sid("s1")));
    assert_eq!(ui_ops(&commands), [LayoutOp::PageSelect { page: page_one }]);
    c.host().ui_apply(LayoutOp::PageSelect { page: page_one });
    c.sync();
    assert_eq!(c.client().focused_pane(), pinned);
    assert_eq!(c.client().focused_session(), Some(sid("s1")));
}

#[test]
fn a_pane_pinning_the_same_id_on_another_machine_is_not_the_sessions() {
    let other = Content::peer_session(shepherd_core::ids::MachineId::new("m_far"), sid("s1"));
    let mut c = status_on_page_two(other);
    let commands = c.user(Intent::StatusAttach(sid("s1")));
    assert_eq!(
        ui_ops(&commands),
        [LayoutOp::PageNew {
            name: None,
            content: Content::Selected,
        }]
    );
}

#[test]
fn attaching_the_selected_session_resizes_it_to_the_pane_newly_focused() {
    let mut c = mixed();
    c.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    let status = only_pane(&c);
    c.host().ui_apply(LayoutOp::Put {
        pane: status,
        content: Content::View(ViewKind::Status),
    });
    c.host().ui_apply(LayoutOp::Split {
        pane: status,
        dir: SplitDir::Vertical,
        ratio: Default::default(),
        content: Content::Selected,
    });
    c.sync();
    c.user(Intent::FocusPane(status));
    assert_eq!(c.client().selected(), Some(&sid("s5")));
    let commands = c.user(Intent::StatusAttach(sid("s5")));
    assert_eq!(
        resized(&commands),
        [sid("s5")],
        "once, to the pane now focused"
    );
    assert_eq!(c.client().focused_session(), Some(sid("s5")));
    let size = c.client().session_size(&sid("s5"));
    let PtySize { cols, rows } = size.unwrap_or(PtySize { cols: 0, rows: 0 });
    assert_eq!(cols, 78, "the bottom pane's inside");
    assert!(rows < 22, "the bottom half, not the whole view: {rows}");
}

#[test]
fn attaching_prefers_a_pane_on_the_current_page() {
    let mut c = status_on_page_two(Content::Selected);
    let status = c.client().focused_pane();
    c.host().ui_apply(LayoutOp::Split {
        pane: status,
        dir: SplitDir::Horizontal,
        ratio: Default::default(),
        content: Content::Selected,
    });
    c.sync();
    let commands = c.user(Intent::StatusAttach(sid("s1")));
    assert!(ui_ops(&commands).is_empty(), "{:?}", ui_ops(&commands));
    let page_two = c.client().host_layout().pages()[1].id;
    assert_eq!(c.client().host_layout().current(), page_two);
    assert_ne!(c.client().focused_pane(), status);
    assert_eq!(c.client().focused_session(), Some(sid("s1")));
}
