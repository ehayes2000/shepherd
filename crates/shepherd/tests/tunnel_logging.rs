//! A tunnel closed for holding too much for a slow client says why in the host's log (F52). Alone in its own test
//! binary: tracing caches each callsite's interest process-wide, so another test hitting the same callsite with no
//! subscriber, at the same moment, could make this test's thread-local subscriber miss it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use shepherd::host::{self, TUNNEL_HELD_MAX, TUNNEL_HIGH};
use shepherd_core::acpp::{Dir, FrameDraft, PeerTrusted, ShepherdFrame, Source, SourceKind};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Link, LinkEvent, LogId};
use shepherd_core::wire::{
    ClientRequest, HostNotification, PtyOutput, Tunnel, encode_host_notification,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::contracts::link::Events;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::FakeNetwork;
use support::{HostHarness, machine_with, response};

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];

#[test]
fn a_tunnel_closed_for_holding_too_much_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let file = host::open_log_file(&root).unwrap();

    // Machine A trusts B; B is a bare link a test drives, and A reads its conn to B only through `read_as`.
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    machines.pace(&a_id);
    let mut trusting = HostDriver::new();
    trusting.append_draft(FrameDraft::new(
        ShepherdFrame::PeerTrusted(PeerTrusted {
            node: b_id.clone(),
            name: "laptop".into(),
        })
        .to_frame(),
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    ));
    let mut a = HostHarness::builder()
        .machines(machines.clone())
        .machine_key(KEY_A)
        .log(FakeEventLog::new().with_envelopes(LogId::Host, trusting.log().to_vec()))
        .start();
    let (b_sink, b_events) = Events::channel();
    let mut b = machines.listen_as(b_id.clone(), b_sink).unwrap();
    machines.trust(&b_id, &a_id);
    let mut client = a.connect_paced();

    tracing::subscriber::with_default(host::file_subscriber(file), || {
        let req = client.request(ClientRequest::Tunnel(Tunnel { machine: b_id }));
        a.step();
        client.read(usize::MAX);
        assert!(response(&client.take(), &req).is_ok());
        let b_conn = match b_events.ready().as_slice() {
            [LinkEvent::Connected { conn }] => *conn,
            other => panic!("expected B to see the tunnel's conn, got {other:?}"),
        };
        let mut line = encode_host_notification(&HostNotification::PtyOutput(PtyOutput {
            session: SessionId::new("s_b"),
            bytes: vec![b'x'; 48 * 1024],
        }));
        line.pop();
        for _ in 0..(TUNNEL_HIGH + TUNNEL_HELD_MAX) / line.len() + 2 {
            b.send(b_conn, &line).unwrap();
            machines.read_as(&a_id, usize::MAX);
        }
        a.step();
    });

    let logged = std::fs::read_to_string(root.join(host::LOG_FILE)).unwrap();
    let why = format!(
        "closing a tunnel: its other end read too slowly, and more than {TUNNEL_HELD_MAX} bytes waited for it"
    );
    assert!(logged.contains(&why), "{logged}");
}
