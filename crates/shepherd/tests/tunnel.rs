//! Tunnels to a peer's host (T31a, decision 0019), end to end through two host runtimes on fakes whose links to other
//! machines share one network: a client of machine A asks A's host for a tunnel to machine B, and from then on speaks
//! to B's host through it as to its own.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::Value;
use shepherd::host::{CATCH_UP_TICK, LOW_WATER, TUNNEL_HIGH};
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, PeerTrusted, RequestId, Seq, ShepherdFrame, Source, SourceKind,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{DisconnectReason, EventLog, Link, LinkEvent, LogId, MAX_QUEUED};
use shepherd_core::wire::{
    ClientRequest, Hello, HostMessage, PROTOCOL_VERSION, PeerList, SessionRename, Tunnel,
    WireError, decode_host_message, encode_request,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::contracts::link::Events;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::FakeNetwork;
use support::*;

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];
const KEY_C: [u8; 32] = [0xcc; 32];

/// The host log of a machine that trusts each of `peers` (id, name).
fn trusting(peers: &[(MachineId, &str)]) -> Vec<Envelope> {
    let mut driver = HostDriver::new();
    for (node, name) in peers {
        let frame = ShepherdFrame::PeerTrusted(PeerTrusted {
            node: node.clone(),
            name: (*name).into(),
        });
        driver.append_draft(FrameDraft::new(
            frame.to_frame(),
            Source::new(SourceKind::Shepherd, None),
            Dir::Internal,
            None,
        ));
    }
    driver.log().to_vec()
}

/// A host for the machine with `key` on `machines`, starting from `log`.
fn host(machines: &FakeNetwork, key: [u8; 32], log: FakeEventLog) -> HostHarness {
    HostHarness::builder()
        .machines(machines.clone())
        .machine_key(key)
        .log(log)
        .start()
}

/// Machine A, trusting B (called "laptop"), and machine B, which lets A in: both hosts on one network.
fn pair() -> (FakeNetwork, HostHarness, HostHarness) {
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    let a_log = FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id, "laptop")]));
    let a = host(&machines, KEY_A, a_log);
    let b = host(&machines, KEY_B, FakeEventLog::new());
    machines.trust(&b.machine(), &a_id);
    (machines, a, b)
}

/// Runs both hosts until neither has anything left to do: the fake network delivers into both.
fn settle(a: &mut HostHarness, b: &mut HostHarness) {
    for _ in 0..4 {
        a.step();
        b.step();
    }
}

fn tunnel_to(client: &mut FakeClient, machine: MachineId) -> RequestId {
    client.request(ClientRequest::Tunnel(Tunnel { machine }))
}

fn hello_machine(msgs: &[HostMessage], id: &RequestId) -> Value {
    response(msgs, id).expect("hello answered")["machine"].clone()
}

fn refusal(msgs: &[HostMessage], id: &RequestId) -> String {
    match response(msgs, id) {
        Err(WireError::InvalidParams(why)) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_client_tunnels_to_a_trusted_machine_and_speaks_to_its_host_as_to_its_own() {
    let (_machines, mut a, mut b) = pair();
    let mut client = a.connect();
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    assert_eq!(
        response(&client.take(), &req),
        Ok(Value::Object(Default::default()))
    );

    let hello = client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("client"),
        cwd: None,
        build: None,
    }));
    settle(&mut a, &mut b);
    assert_eq!(hello_machine(&client.take(), &hello), b.machine().as_str());

    let new = client.new_session();
    settle(&mut a, &mut b);
    let session = new_session_id(&client.take(), &new);
    let has = |h: &HostHarness| h.host().sessions().iter().any(|s| s.id == session);
    assert!(has(&b), "made on B");
    assert!(!has(&a), "not on A");
}

#[test]
fn a_client_that_hangs_up_closes_its_tunnel_and_the_other_host_carries_on() {
    let (_machines, mut a, mut b) = pair();
    let mut gone = a.connect();
    tunnel_to(&mut gone, b.machine());
    settle(&mut a, &mut b);
    gone.close();
    settle(&mut a, &mut b);
    // B's host serves a new tunnel as before.
    let mut client = a.connect();
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    assert!(response(&client.take(), &req).is_ok());
    assert!(!client.is_disconnected());
}

#[test]
fn a_tunnel_to_a_machine_this_one_does_not_trust_is_refused_and_closed() {
    let machines = FakeNetwork::new();
    let mut a = host(&machines, KEY_A, FakeEventLog::new());
    let mut b = host(&machines, KEY_B, FakeEventLog::new());
    machines.trust(&b.machine(), &a.machine());
    let mut client = a.connect();
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    let why = refusal(&client.take(), &req);
    assert!(why.contains("no trusted machine"), "{why}");
    assert!(client.is_disconnected());
}

#[test]
fn a_tunnel_to_a_machine_that_does_not_trust_this_one_says_so_and_is_closed() {
    let machines = FakeNetwork::new();
    let b_id = machine_with(KEY_B);
    let a_log = FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id, "laptop")]));
    let mut a = host(&machines, KEY_A, a_log);
    let mut b = host(&machines, KEY_B, FakeEventLog::new());
    let mut client = a.connect();
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    assert_eq!(
        refusal(&client.take(), &req),
        "laptop doesn't trust this machine: pair them again with `shepherd pair`"
    );
    assert!(client.is_disconnected());
}

#[test]
fn a_tunnel_to_a_trusted_machine_that_cannot_be_reached_says_so_and_is_closed() {
    let machines = FakeNetwork::new();
    let c_id = machine_with(KEY_C);
    let a_log =
        FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(c_id.clone(), "tower")]));
    let mut a = host(&machines, KEY_A, a_log);
    let mut client = a.connect();
    let req = tunnel_to(&mut client, c_id);
    a.step();
    assert_eq!(refusal(&client.take(), &req), "cannot reach tower");
    assert!(client.is_disconnected());
}

#[test]
fn a_tunnel_after_another_request_is_refused_and_the_conn_kept() {
    let (_machines, mut a, mut b) = pair();
    let mut client = a.connect();
    client.request(ClientRequest::PeerList(PeerList {}));
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    let why = refusal(&client.take(), &req);
    assert!(why.contains("first request"), "{why}");
    assert!(!client.is_disconnected());
    let list = client.request(ClientRequest::PeerList(PeerList {}));
    a.step();
    assert!(response(&client.take(), &list).is_ok());
}

/// A conn from another machine's host: it may not tunnel on through this one, but stays an ordinary client.
#[test]
fn a_tunnel_asked_for_by_another_machine_is_refused_and_the_conn_kept() {
    let (machines, mut a, mut b) = pair();
    let c_id = machine_with(KEY_C);
    machines.trust(&a.machine(), &c_id);
    let (sink, events) = Events::channel();
    let mut c = machines.listen_as(c_id, sink).unwrap();
    let conn = c.open(&a.machine()).unwrap();
    let line = encode_request(&shepherd_core::wire::Request {
        id: 1.into(),
        body: ClientRequest::Tunnel(Tunnel {
            machine: b.machine(),
        }),
    });
    c.send(conn, line.strip_suffix(b"\n").unwrap()).unwrap();
    settle(&mut a, &mut b);
    let msgs: Vec<HostMessage> = events
        .ready()
        .into_iter()
        .filter_map(|e| match e {
            LinkEvent::Line { line, .. } => Some(decode_host_message(&line).unwrap()),
            LinkEvent::Disconnected { .. } => panic!("closed"),
            LinkEvent::Connected { .. } => None,
        })
        .collect();
    let why = refusal(&msgs, &1.into());
    assert!(why.contains("this machine's own clients"), "{why}");
}

#[test]
fn a_tunnel_whose_machine_goes_away_is_closed_with_peer_gone() {
    let (_machines, mut a, mut b) = pair();
    let mut client = a.connect();
    tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    assert!(!client.is_disconnected());
    drop(b);
    a.step();
    assert_eq!(client.disconnect_reason(), Some(DisconnectReason::PeerGone));
}

/// A previous host's log for session `s_a`: created, then renamed until it holds more than `bytes` of 8 KiB names.
fn big_history(bytes: usize) -> FakeEventLog {
    let name = "n".repeat(8 * 1024);
    let mut prev = HostDriver::new();
    prev.new_session("s_a");
    for i in 0..bytes / name.len() + 1 {
        prev.rename("s_a", &format!("{i}-{name}"));
    }
    FakeEventLog::new().with_envelopes(LogId::Session(SessionId::new("s_a")), prev.log().to_vec())
}

/// A slow client of A catching up on B's large history through a tunnel. A's host reads its conn to B a chunk at a
/// time, as a link's reader does: while the client is behind, A stops reading it, so B's own queue fills and B's
/// catch-up waits, instead of A piling B's history up in the client's queue (which would drop it as a slow consumer).
#[test]
fn a_slow_client_holds_back_the_other_hosts_catch_up_through_the_tunnel() {
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    machines.pace(&a_id);
    let a_log = FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id, "laptop")]));
    let mut a = host(&machines, KEY_A, a_log);
    let mut b = host(&machines, KEY_B, big_history(MAX_QUEUED * 3 / 2));
    machines.trust(&b.machine(), &a_id);

    let mut slow = a.connect_paced();
    let tunnel = tunnel_to(&mut slow, b.machine());
    settle(&mut a, &mut b);
    slow.read(usize::MAX);
    assert!(response(&slow.take(), &tunnel).is_ok());
    let req = slow.subscribe_all();

    let line = 8 * 1024 + 1024;
    let (mut msgs, mut rounds) = (Vec::new(), 0);
    let (mut most_at_client, mut most_at_b) = (0, 0);
    while !msgs
        .iter()
        .any(|m| matches!(m, HostMessage::Response(r) if r.id == req))
    {
        rounds += 1;
        assert!(rounds < 10_000, "catch-up never finished");
        assert!(!slow.is_disconnected(), "dropped after {rounds} rounds");
        machines.read_as(&a_id, 256 * 1024);
        settle(&mut a, &mut b);
        most_at_client = most_at_client.max(slow.unread());
        most_at_b = most_at_b.max(machines.unread_by(&a_id));
        slow.read(32 * 1024);
        msgs.extend(slow.take());
        a.clock().advance(CATCH_UP_TICK);
        b.clock().advance(CATCH_UP_TICK);
    }
    assert!(
        most_at_client <= TUNNEL_HIGH + line,
        "the client's queue reached {most_at_client}"
    );
    assert!(
        most_at_b >= LOW_WATER,
        "B's catch-up never waited on A: at most {most_at_b} queued"
    );
    let seqs: Vec<u64> = entries(&msgs)
        .iter()
        .filter(|e| e.session().map(SessionId::as_str) == Some("s_a"))
        .map(|e| e.seq().get())
        .collect();
    let total = b
        .log()
        .read(LogId::Session(SessionId::new("s_a")), Seq::ZERO)
        .unwrap()
        .len() as u64;
    assert_eq!(seqs, (1..=total).collect::<Vec<_>>());
}

/// The other way: B's host reads its end of the tunnel slowly, so A stops reading the client, whose own queue fills,
/// instead of piling what it sends up in the conn to B.
#[test]
fn a_host_that_reads_slowly_holds_back_a_client_sending_through_the_tunnel() {
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    machines.pace(&b_id);
    let a_log =
        FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id.clone(), "laptop")]));
    let mut a = host(&machines, KEY_A, a_log);
    let mut b = host(&machines, KEY_B, FakeEventLog::new());
    machines.trust(&b_id, &a_id);
    a.network().pace_host();
    let mut client = a.connect();
    let tunnel = tunnel_to(&mut client, b_id.clone());
    a.network().read_host(usize::MAX);
    settle(&mut a, &mut b);
    assert!(response(&client.take(), &tunnel).is_ok());

    // Renames of a session B doesn't have: each is answered with an error, which is all the client needs.
    let name = "r".repeat(8 * 1024);
    // Twice what the tunnel lets wait for B, and within what the client may queue itself.
    let sent: Vec<RequestId> = (0..MAX_QUEUED / 2 / name.len())
        .map(|i| {
            client.request(ClientRequest::SessionRename(SessionRename {
                session: SessionId::new("s_nobody"),
                name: format!("{i}-{name}"),
            }))
        })
        .collect();
    let line = name.len() + 1024;
    let total = client.queued();
    let chunk = 256 * 1024;
    let (mut msgs, mut rounds, mut read_by_b) = (Vec::new(), 0, 0);
    while msgs.len() < sent.len() {
        rounds += 1;
        assert!(rounds < 10_000, "never all answered");
        assert!(!client.is_disconnected(), "dropped after {rounds} rounds");
        // A reads its client a chunk at a time while it isn't stopped, B reads its end of the tunnel slower.
        a.network().read_host(chunk);
        settle(&mut a, &mut b);
        let read_by_a = total - client.queued();
        assert!(
            read_by_a <= read_by_b + TUNNEL_HIGH + chunk + line,
            "round {rounds}: A read {read_by_a} of the client, B only {read_by_b} of A"
        );
        read_by_b += machines.read_as(&b_id, 32 * 1024);
        settle(&mut a, &mut b);
        msgs.extend(client.take());
        a.clock().advance(CATCH_UP_TICK);
        b.clock().advance(CATCH_UP_TICK);
    }
    let answered: Vec<RequestId> = msgs
        .iter()
        .filter_map(|m| match m {
            HostMessage::Response(r) => Some(r.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(answered, sent);
}
