//! Tunnels to a peer's host (T31a, decision 0019), end to end through two host runtimes on fakes whose links to other
//! machines share one network: a client of machine A asks A's host for a tunnel to machine B, and from then on speaks
//! to B's host through it as to its own.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::Value;
use shepherd::host::{
    CATCH_UP_TICK, LOW_WATER, MAX_TUNNEL_TICK, RuntimeEvent, TUNNEL_HELD_MAX, TUNNEL_HIGH,
    TUNNEL_LOW,
};
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, PeerTrusted, RequestId, Seq, ShepherdFrame, Source, SourceKind,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    ConnId, DisconnectReason, EventLog, Link, LinkError, LinkEvent, LogId, MAX_OPEN_PER_PEER,
    MAX_QUEUED,
};
use shepherd_core::wire::{
    ClientRequest, Hello, HostMessage, HostNotification, HostUpgrade, PROTOCOL_VERSION, PairJoin,
    PairStart, PairWait, PeerList, PeerRemove, PtyOutput, SessionRename, Tunnel, WireError,
    decode_host_message, encode_host_notification, encode_request,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::contracts::link::Events;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
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

/// Machine C, trusted by A (of [`pair`]), on a conn of its own to A's host, as another machine's host or tunnel would be.
fn peer_conn(machines: &FakeNetwork, a: &HostHarness) -> (FakeLink, Events, ConnId) {
    let c_id = machine_with(KEY_C);
    machines.trust(&a.machine(), &c_id);
    let (sink, events) = Events::channel();
    let mut c = machines.listen_as(c_id, sink).unwrap();
    let conn = c.open(&a.machine()).unwrap();
    (c, events, conn)
}

/// Sends `body` as request 1 on C's conn to A and returns what A answered, the conn staying open.
fn ask_from_peer(body: ClientRequest) -> (HostHarness, Vec<HostMessage>) {
    let (machines, mut a, mut b) = pair();
    let (mut c, events, conn) = peer_conn(&machines, &a);
    let line = encode_request(&shepherd_core::wire::Request { id: 1.into(), body });
    c.send(conn, line.strip_suffix(b"\n").unwrap()).unwrap();
    settle(&mut a, &mut b);
    let msgs = events
        .ready()
        .into_iter()
        .filter_map(|e| match e {
            LinkEvent::Line { line, .. } => Some(decode_host_message(&line).unwrap()),
            LinkEvent::Disconnected { .. } => panic!("closed"),
            LinkEvent::Connected { .. } => None,
        })
        .collect();
    (a, msgs)
}

fn assert_refused_from_peer(body: ClientRequest) -> HostHarness {
    let (a, msgs) = ask_from_peer(body);
    let why = refusal(&msgs, &1.into());
    assert!(why.contains("this machine's own clients"), "{why}");
    a
}

#[test]
fn another_machine_may_not_start_a_pairing() {
    let a = assert_refused_from_peer(ClientRequest::PairStart(PairStart {}));
    assert!(!a.peer_net().is_pairing(&a.machine()));
}

#[test]
fn another_machine_may_not_wait_on_a_pairing() {
    assert_refused_from_peer(ClientRequest::PairWait(PairWait {}));
}

#[test]
fn another_machine_may_not_join_a_pairing_for_this_one() {
    assert_refused_from_peer(ClientRequest::PairJoin(PairJoin {
        ticket: "a ticket".into(),
    }));
}

#[test]
fn another_machine_may_not_remove_a_trusted_peer() {
    let mut a = assert_refused_from_peer(ClientRequest::PeerRemove(PeerRemove {
        peer: "laptop".into(),
    }));
    let mut client = a.connect();
    let list = client.request(ClientRequest::PeerList(PeerList {}));
    a.step();
    let peers = response(&client.take(), &list).unwrap();
    assert_eq!(peers["peers"][0]["name"], "laptop", "{peers}");
}

#[test]
fn another_machine_may_not_update_this_machines_host() {
    let mut a = assert_refused_from_peer(ClientRequest::HostUpgrade(HostUpgrade { force: true }));
    assert!(a.runtime_mut().take_swap().is_none());
}

/// Another machine's conn that asks for a tunnel and goes before the host reads the request: the host no longer
/// knows whose conn it was, so it must not take it for a local one and dial on its behalf.
#[test]
fn a_tunnel_asked_for_by_another_machine_that_left_at_once_dials_nobody() {
    let machines = FakeNetwork::new();
    let (a_id, b_id, c_id) = (
        machine_with(KEY_A),
        machine_with(KEY_B),
        machine_with(KEY_C),
    );
    let a_log =
        FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id.clone(), "laptop")]));
    let mut a = host(&machines, KEY_A, a_log);
    // B is a bare link, so any conn A opens to it shows.
    let (b_sink, b_events) = Events::channel();
    let _b = machines.listen_as(b_id.clone(), b_sink).unwrap();
    machines.trust(&b_id, &a_id);
    machines.trust(&a_id, &c_id);
    let (sink, _events) = Events::channel();
    let mut c = machines.listen_as(c_id, sink).unwrap();
    let conn = c.open(&a_id).unwrap();
    let line = encode_request(&shepherd_core::wire::Request {
        id: 1.into(),
        body: ClientRequest::Tunnel(Tunnel { machine: b_id }),
    });
    c.send(conn, line.strip_suffix(b"\n").unwrap()).unwrap();
    c.close(conn).unwrap();
    a.step();
    assert_eq!(
        b_events.ready(),
        vec![],
        "A dialled B for a conn it no longer knew"
    );
}

/// One connection to a machine carries at most [`MAX_OPEN_PER_PEER`] tunnels at once: one more is refused with why at
/// once, rather than waiting on the connection for a stream that won't come. Closing one makes room.
#[test]
fn a_tunnel_past_the_most_one_connection_carries_is_refused_until_one_closes() {
    let (_machines, mut a, mut b) = pair();
    let mut open: Vec<FakeClient> = (0..MAX_OPEN_PER_PEER)
        .map(|_| {
            let mut client = a.connect();
            tunnel_to(&mut client, b.machine());
            client
        })
        .collect();
    settle(&mut a, &mut b);
    assert!(open.iter_mut().all(|c| !c.is_disconnected()));

    let mut one_more = a.connect();
    let req = tunnel_to(&mut one_more, b.machine());
    settle(&mut a, &mut b);
    let why = refusal(&one_more.take(), &req);
    assert_eq!(
        why,
        format!(
            "laptop already has {MAX_OPEN_PER_PEER} tunnels open from this machine, the most one connection carries: \
             close one first"
        )
    );
    assert!(one_more.is_disconnected());

    open.pop().unwrap().close();
    settle(&mut a, &mut b);
    let mut client = a.connect();
    let req = tunnel_to(&mut client, b.machine());
    settle(&mut a, &mut b);
    assert!(response(&client.take(), &req).is_ok());
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

/// A tunnel from a slow (paced) client of A to a bare link standing in for B's host, which a test drives line by line.
/// A reads its conn to B only through `FakeNetwork::read_as`.
struct Bare {
    machines: FakeNetwork,
    a: HostHarness,
    a_id: MachineId,
    b: FakeLink,
    b_events: Events,
    /// B's end of the tunnel.
    b_conn: shepherd_core::ports::ConnId,
    client: FakeClient,
}

fn bare_tunnel() -> Bare {
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    machines.pace(&a_id);
    let a_log =
        FakeEventLog::new().with_envelopes(LogId::Host, trusting(&[(b_id.clone(), "laptop")]));
    let mut a = host(&machines, KEY_A, a_log);
    let (b_sink, b_events) = Events::channel();
    let b = machines.listen_as(b_id.clone(), b_sink).unwrap();
    machines.trust(&b_id, &a_id);
    let mut client = a.connect_paced();
    let req = tunnel_to(&mut client, b_id);
    a.step();
    client.read(usize::MAX);
    assert!(response(&client.take(), &req).is_ok());
    let b_conn = match b_events.ready().as_slice() {
        [LinkEvent::Connected { conn }] => *conn,
        other => panic!("expected B to see the tunnel's conn, got {other:?}"),
    };
    Bare {
        machines,
        a,
        a_id,
        b,
        b_events,
        b_conn,
        client,
    }
}

/// One line as B's host would send it: output of `len` bytes.
fn output_line(len: usize) -> Vec<u8> {
    let note = HostNotification::PtyOutput(PtyOutput {
        session: SessionId::new("s_b"),
        bytes: vec![b'x'; len],
    });
    let mut line = encode_host_notification(&note);
    line.pop();
    line
}

fn outputs(msgs: &[HostMessage]) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, HostMessage::Notification(HostNotification::PtyOutput(_))))
        .count()
}

impl Bare {
    /// Moves A's clock to its next deadline and steps it.
    fn next_deadline(&mut self) -> std::time::Duration {
        let clock = self.a.clock();
        let due = self.a.runtime().next_deadline().expect("a deadline");
        let wait = due.saturating_duration_since(clock.now());
        clock.advance(wait);
        self.a.step();
        wait
    }
}

#[test]
fn lines_held_for_a_slow_client_reach_it_before_the_other_host_is_gone() {
    let mut t = bare_tunnel();
    let n = 100;
    for _ in 0..n {
        t.b.send(t.b_conn, &output_line(48 * 1024)).unwrap();
    }
    t.machines.read_as(&t.a_id, usize::MAX);
    t.a.step();
    // A took them all from B, but only as many as the tunnel lets wait went on to the client: the rest are held.
    assert_eq!(t.machines.unread_by(&t.a_id), 0);
    assert!(t.client.unread() <= TUNNEL_HIGH, "{}", t.client.unread());
    drop(t.b);
    t.a.step();
    t.client.read(usize::MAX);
    assert_eq!(outputs(&t.client.take()), n);
    assert_eq!(
        t.client.disconnect_reason(),
        Some(DisconnectReason::PeerGone)
    );
}

/// Lines handed to A before it stopped reading B are held for the client, but only up to [`TUNNEL_HELD_MAX`] bytes:
/// past that the client is a slow consumer, and the tunnel is closed on both ends rather than held without bound.
#[test]
fn a_tunnel_holding_more_than_its_cap_for_a_slow_client_is_closed() {
    let mut t = bare_tunnel();
    let line = output_line(48 * 1024);
    let n = (TUNNEL_HIGH + TUNNEL_HELD_MAX) / line.len() + 2;
    // Each is handed to A as B sends it, before A runs at all, so B never has much queued itself.
    for _ in 0..n {
        t.b.send(t.b_conn, &line).unwrap();
        t.machines.read_as(&t.a_id, usize::MAX);
    }
    t.a.step();
    assert!(
        t.b_events
            .ready()
            .iter()
            .any(|e| matches!(e, LinkEvent::Disconnected { conn, .. } if *conn == t.b_conn)),
        "B's end is still open"
    );
    t.client.read(usize::MAX);
    let delivered = outputs(&t.client.take());
    assert!(delivered < n, "all {n} delivered");
    assert!(t.client.is_disconnected());
}

/// A tunnel end is stopped only once its other end's queue is at [`TUNNEL_HIGH`] (or a line doesn't fit under it), and
/// read again only once that queue is under [`TUNNEL_LOW`].
#[test]
fn a_tunnel_stops_reading_at_the_high_mark_and_reads_again_under_the_low_mark() {
    let mut t = bare_tunnel();
    let line = output_line(32 * 1024);
    // B sends a line at a time, and A reads one at a time while it reads at all.
    loop {
        t.b.send(t.b_conn, &line).unwrap();
        if t.machines.read_as(&t.a_id, 1) == 0 {
            break;
        }
        t.a.step();
        assert!(
            t.client.unread() < TUNNEL_HIGH + line.len(),
            "never stopped"
        );
    }
    assert!(
        t.client.unread() + line.len() > TUNNEL_HIGH,
        "stopped with {} queued for the client",
        t.client.unread()
    );
    // The client reads a line at a time; A looks again at each deadline.
    let resumed_at = loop {
        t.client.read(1);
        t.next_deadline();
        let queued = t.client.unread();
        if t.machines.read_as(&t.a_id, 1) > 0 {
            break queued;
        }
        assert!(queued > 0, "never read again");
    };
    assert!(
        resumed_at < TUNNEL_LOW,
        "read again with {resumed_at} queued for the client"
    );
}

/// One line bigger than the high mark goes into the client's empty queue, and that alone stops the tunnel.
#[test]
fn a_tunnel_stops_reading_once_one_huge_line_fills_the_queue() {
    let mut t = bare_tunnel();
    t.b.send(t.b_conn, &output_line(TUNNEL_HIGH)).unwrap();
    t.b.send(t.b_conn, &output_line(16)).unwrap();
    assert!(t.machines.read_as(&t.a_id, 1) > 0);
    t.a.step();
    assert!(t.client.unread() >= TUNNEL_HIGH);
    assert_eq!(t.machines.read_as(&t.a_id, 1), 0, "still reading B");
}

/// A client that stopped reading altogether (a suspended TUI) costs few wakeups: the stopped tunnel's checks back off.
#[test]
fn a_stopped_tunnel_whose_client_never_reads_backs_off_its_checks() {
    let mut t = bare_tunnel();
    for _ in 0..100 {
        t.b.send(t.b_conn, &output_line(48 * 1024)).unwrap();
    }
    t.machines.read_as(&t.a_id, usize::MAX);
    t.a.step();
    let mut waits = Vec::new();
    while waits.iter().sum::<std::time::Duration>() < std::time::Duration::from_secs(60) {
        waits.push(t.next_deadline());
    }
    let ms = std::time::Duration::from_millis;
    assert_eq!(
        waits[..8],
        [
            ms(5),
            ms(10),
            ms(20),
            ms(40),
            ms(80),
            ms(160),
            ms(320),
            ms(500)
        ]
    );
    // At most one check per `MAX_TUNNEL_TICK` from then on (the peering's redials of B wake it now and then too).
    let checks = (60_000 / MAX_TUNNEL_TICK.as_millis()) as usize;
    assert!(
        waits.len() < checks + 20,
        "{} wakeups in a minute",
        waits.len()
    );
}

#[test]
fn shutting_down_delivers_what_a_tunnelled_client_was_sent_then_closes_both_ends() {
    let mut t = bare_tunnel();
    for _ in 0..10 {
        t.b.send(t.b_conn, &output_line(1024)).unwrap();
    }
    t.machines.read_as(&t.a_id, usize::MAX);
    t.a.step();
    t.a.send(RuntimeEvent::Shutdown);
    t.client.read(usize::MAX);
    assert_eq!(outputs(&t.client.take()), 10);
    assert_eq!(
        t.client.disconnect_reason(),
        Some(DisconnectReason::PeerGone)
    );
    assert!(
        t.b_events
            .ready()
            .iter()
            .any(|e| matches!(e, LinkEvent::Disconnected { conn, .. } if *conn == t.b_conn)),
        "B's end is still open"
    );
}

#[test]
fn a_tunnel_the_link_cannot_open_says_the_machine_cannot_be_reached_and_is_closed() {
    let (_machines, mut a, _b) = pair();
    a.runtime_mut()
        .link_mut()
        .peers_mut()
        .fail_next(LinkError::NoPeers);
    let mut client = a.connect();
    let req = tunnel_to(&mut client, machine_with(KEY_B));
    a.step();
    assert_eq!(refusal(&client.take(), &req), "cannot reach laptop");
    assert!(client.is_disconnected());
}
