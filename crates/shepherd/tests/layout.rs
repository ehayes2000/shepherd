//! The host-owned screen layout (T48, decision 0022) through the host runtime, on all-fake ports: `_shepherd/ui/get` and
//! `_shepherd/ui/apply`, broadcast to subscribed clients, kept across a restart and a hot swap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::Value;
use shepherd_core::acpp::{Frame, ShepherdFrame};
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, Pane, PaneId, SplitDir};
use shepherd_core::wire::{
    BuildId, ClientRequest, DigestHasher, HostUpgrade, UiApply, UiGet, UpgradeAnswer, WireError,
};
use support::{FakeClient, HostHarness, entries, new_session_id, response};

fn first_pane() -> PaneId {
    match Layout::default().current_page().root {
        Pane::Leaf { id, .. } => id,
        Pane::Split { .. } => unreachable!("the default is one pane"),
    }
}

fn show(session: &SessionId) -> LayoutOp {
    LayoutOp::Put {
        pane: first_pane(),
        content: Content::Session(session.clone()),
    }
}

fn layout(value: Value) -> Layout {
    serde_json::from_value(value).expect("a layout")
}

/// Asks the host for its layout.
fn get(h: &mut HostHarness, client: &mut FakeClient) -> Layout {
    let req = client.request(ClientRequest::UiGet(UiGet {}));
    h.step();
    layout(response(&client.take(), &req).expect("ui/get answered"))
}

fn apply(h: &mut HostHarness, client: &mut FakeClient, op: LayoutOp) -> Result<Layout, WireError> {
    let req = client.request(ClientRequest::UiApply(UiApply { op }));
    h.step();
    response(&client.take(), &req).map(layout)
}

fn new_session(h: &mut HostHarness, client: &mut FakeClient) -> SessionId {
    let req = client.new_session();
    h.step();
    new_session_id(&client.take(), &req)
}

#[test]
fn an_applied_op_is_answered_and_every_subscribed_client_is_sent_the_new_layout() {
    let mut h = HostHarness::new();
    let mut watcher = h.connect();
    watcher.subscribe_all();
    let mut client = h.connect();
    let session = new_session(&mut h, &mut client);
    watcher.take();

    let applied = apply(&mut h, &mut client, show(&session)).expect("applied");

    let expected = Layout::default().apply(&show(&session), |_| true).unwrap();
    assert_eq!(applied, expected);
    let sent: Vec<Layout> = entries(&watcher.take())
        .iter()
        .filter_map(|env| match env.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::UiLayout(recorded) => Some(recorded.layout.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(sent, std::slice::from_ref(&expected));
    assert_eq!(get(&mut h, &mut client), expected);
}

#[test]
fn a_refused_op_is_answered_with_why() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let refused = apply(
        &mut h,
        &mut client,
        LayoutOp::Split {
            pane: PaneId::new(404),
            dir: SplitDir::Horizontal,
            ratio: Default::default(),
            content: Content::Empty,
        },
    );
    assert_eq!(refused, Err(WireError::InvalidParams("no pane 404".into())));
    assert_eq!(get(&mut h, &mut client), Layout::default());
}

#[test]
fn the_layout_survives_a_restart() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let session = new_session(&mut h, &mut client);
    let applied = apply(&mut h, &mut client, show(&session)).unwrap();
    assert_eq!(
        applied,
        Layout::default().apply(&show(&session), |_| true).unwrap()
    );

    h.restart();
    let mut client = h.connect();
    assert_eq!(get(&mut h, &mut client), applied);
    assert_eq!(h.host().layout(), applied);
}

#[test]
fn the_layout_survives_a_hot_swap() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let mut hasher = DigestHasher::new();
    hasher.update(b"the build the host started as");
    let running = BuildId {
        version: "0.1.0".into(),
        digest: hasher.finish(),
    };
    let mut h = HostHarness::builder().build(running, &exe).start();
    let mut client = h.connect();
    let session = new_session(&mut h, &mut client);
    let applied = apply(&mut h, &mut client, show(&session)).unwrap();
    assert_eq!(
        applied,
        Layout::default().apply(&show(&session), |_| true).unwrap()
    );
    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.step();
    let answer: UpgradeAnswer =
        serde_json::from_value(response(&client.take(), &req).unwrap()).unwrap();
    assert_eq!(answer, UpgradeAnswer::Accepted);

    h.swap(|_| {});

    let mut client = h.connect();
    assert_eq!(
        get(&mut h, &mut client),
        applied,
        "the adopted session is still shown"
    );
}
