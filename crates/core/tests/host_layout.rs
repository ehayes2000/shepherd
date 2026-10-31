//! The host owns the screen layout (decision 0022): `_shepherd/ui/get` and `_shepherd/ui/apply`, folded from the host
//! log.

use std::collections::{BTreeMap, VecDeque};

use serde_json::json;
use shepherd_core::acpp::Envelope;
use shepherd_core::host::{Host, HostCommand, HostEvent};
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, Pane, PaneId, SplitDir};
use shepherd_core::wire::{HostResult, WireError};
use shepherd_testkit::builders::HostDriver;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

/// The single pane of the current page.
fn only_pane(layout: &Layout) -> PaneId {
    match &layout.current_page().root {
        Pane::Leaf { id, .. } => *id,
        other => panic!("not one pane: {other:?}"),
    }
}

fn put(pane: PaneId, session: &str) -> LayoutOp {
    LayoutOp::Put {
        pane,
        content: Content::session(sid(session)),
    }
}

/// The reply in `commands`.
fn reply(commands: &[HostCommand]) -> &Result<HostResult, WireError> {
    commands
        .iter()
        .find_map(|c| match c {
            HostCommand::Reply { result, .. } => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no reply in {commands:?}"))
}

/// The layout frames appended to the host log by `commands`.
fn layout_appends(commands: &[HostCommand]) -> Vec<serde_json::Value> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::Append(draft) if draft.session.is_none() => (draft.frame.method()
                == Some("_shepherd/ui/layout"))
            .then(|| draft.frame.raw()["params"].clone()),
            _ => None,
        })
        .collect()
}

fn apply(driver: &mut HostDriver, op: LayoutOp) -> Vec<HostCommand> {
    let reply = driver.reply_to();
    driver.handle(HostEvent::UiApply { reply, op })
}

fn replay(envs: impl IntoIterator<Item = Envelope>) -> Host {
    let mut host = Host::new();
    for env in envs {
        host.apply(&env);
    }
    host
}

#[test]
fn a_host_with_no_layout_recorded_has_the_default() {
    let mut driver = HostDriver::new();
    assert_eq!(driver.host().layout(), Layout::default());
    let reply_to = driver.reply_to();
    let commands = driver.handle(HostEvent::UiGet { reply: reply_to });
    assert_eq!(
        commands,
        vec![HostCommand::Reply {
            to: reply_to,
            result: Ok(HostResult::Layout(Layout::default())),
        }]
    );
}

#[test]
fn an_applied_op_is_recorded_in_the_host_log_and_answered_with_the_new_layout() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    let pane = only_pane(&Layout::default());
    let commands = apply(&mut driver, put(pane, "s1"));
    let expected = Layout::default().apply(&put(pane, "s1"), |_| true).unwrap();
    assert_eq!(reply(&commands), &Ok(HostResult::Layout(expected.clone())));
    assert_eq!(
        layout_appends(&commands),
        [json!({ "layout": serde_json::to_value(&expected).unwrap() })]
    );
    assert_eq!(driver.host().layout(), expected);
    // `ui/get` answers it too.
    let reply_to = driver.reply_to();
    let get = driver.handle(HostEvent::UiGet { reply: reply_to });
    assert_eq!(reply(&get), &Ok(HostResult::Layout(expected)));
}

#[test]
fn a_refused_op_is_answered_with_why_and_records_nothing() {
    let mut driver = HostDriver::new();
    let before = driver.log().len();
    let commands = apply(
        &mut driver,
        LayoutOp::Close {
            pane: PaneId::new(99),
        },
    );
    assert_eq!(
        reply(&commands),
        &Err(WireError::InvalidParams("no pane 99".into()))
    );
    assert_eq!(driver.log().len(), before);
    assert_eq!(driver.host().layout(), Layout::default());
}

#[test]
fn only_open_sessions_can_be_put_in_a_pane() {
    let mut driver = HostDriver::new();
    let pane = only_pane(&Layout::default());
    let unknown = apply(&mut driver, put(pane, "nope"));
    assert_eq!(
        reply(&unknown),
        &Err(WireError::InvalidParams("no session nope".into()))
    );
    driver.new_session("s1");
    driver.close("s1");
    assert!(driver.host().sessions().is_empty(), "s1 closed");
    let closed = apply(&mut driver, put(pane, "s1"));
    assert_eq!(
        reply(&closed),
        &Err(WireError::InvalidParams("no session s1".into()))
    );
}

#[test]
fn an_op_that_changes_nothing_records_nothing() {
    let mut driver = HostDriver::new();
    let page = Layout::default().current();
    let before = driver.log().len();
    let commands = apply(&mut driver, LayoutOp::PageSelect { page });
    assert_eq!(reply(&commands), &Ok(HostResult::Layout(Layout::default())));
    assert_eq!(driver.log().len(), before);
}

#[test]
fn a_closed_sessions_panes_show_nothing() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    driver.new_session("s2");
    let a = only_pane(&Layout::default());
    apply(&mut driver, put(a, "s1"));
    let split = LayoutOp::Split {
        pane: a,
        dir: SplitDir::Horizontal,
        ratio: Default::default(),
        content: Content::session(sid("s2")),
    };
    apply(&mut driver, split);
    driver.close("s1");
    let layout = driver.host().layout();
    let Pane::Split {
        a: left, b: right, ..
    } = &layout.current_page().root
    else {
        panic!("{layout:?}");
    };
    assert_eq!(
        **left,
        Pane::Leaf {
            id: a,
            content: Content::Empty
        }
    );
    assert!(
        matches!(&**right, Pane::Leaf { content: Content::Session { machine: None, id }, .. } if *id == sid("s2"))
    );
}

#[test]
fn the_layout_survives_a_restart() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    let pane = only_pane(&Layout::default());
    apply(&mut driver, put(pane, "s1"));
    apply(
        &mut driver,
        LayoutOp::PageNew {
            name: Some("two".into()),
            content: Content::Empty,
        },
    );
    let replayed = replay(driver.log().iter().cloned());
    assert_eq!(replayed.layout(), driver.host().layout());
    assert_eq!(replayed.layout().pages().len(), 2);
}

#[test]
fn the_layout_folds_the_same_whichever_log_is_replayed_first() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    driver.new_session("s2");
    let pane = only_pane(&Layout::default());
    apply(&mut driver, put(pane, "s1"));
    driver.close("s1");
    apply(
        &mut driver,
        LayoutOp::Split {
            pane,
            dir: SplitDir::Vertical,
            ratio: Default::default(),
            content: Content::session(sid("s2")),
        },
    );
    let mut logs: BTreeMap<Option<SessionId>, VecDeque<Envelope>> = BTreeMap::new();
    for env in driver.log() {
        logs.entry(env.session().cloned())
            .or_default()
            .push_back(env.clone());
    }
    let logs: Vec<VecDeque<Envelope>> = logs.into_values().collect();
    let forwards = replay(logs.clone().into_iter().flatten());
    let backwards = replay(logs.into_iter().rev().flatten());
    assert_eq!(forwards.layout(), driver.host().layout());
    assert_eq!(backwards.layout(), driver.host().layout());
}

#[test]
fn a_layout_frame_in_a_session_log_is_ignored() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    let other = Layout::default()
        .apply(
            &LayoutOp::PageNew {
                name: None,
                content: Content::Empty,
            },
            |_| true,
        )
        .unwrap();
    driver.append(
        Some("s1"),
        shepherd_core::acpp::Source::new(shepherd_core::acpp::SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/ui/layout", "params": { "layout": other } }),
    );
    assert_eq!(driver.host().layout(), Layout::default());
}

#[test]
fn a_malformed_layout_frame_is_skipped() {
    let mut driver = HostDriver::new();
    driver.append(
        None,
        shepherd_core::acpp::Source::new(shepherd_core::acpp::SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/ui/layout", "params": { "layout": { "pages": [], "current": 1, "next_id": 2 } } }),
    );
    assert_eq!(driver.host().layout(), Layout::default());
}

#[test]
fn a_peers_session_can_be_pinned_and_stays_when_this_hosts_sessions_close() {
    let mut driver = HostDriver::new();
    driver.new_session("s1");
    let pane = only_pane(&Layout::default());
    let theirs = Content::peer_session(shepherd_core::ids::MachineId::new("m_b"), sid("s9"));
    let commands = apply(
        &mut driver,
        LayoutOp::Put {
            pane,
            content: theirs.clone(),
        },
    );
    assert!(reply(&commands).is_ok(), "{commands:?}");
    driver.close("s1");
    let replayed = replay(driver.log().iter().cloned());
    for layout in [driver.host().layout(), replayed.layout()] {
        assert_eq!(
            layout.current_page().root,
            Pane::Leaf {
                id: pane,
                content: theirs.clone()
            }
        );
    }
}
