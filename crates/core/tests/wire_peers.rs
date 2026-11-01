//! The pairing and peer requests (T30) on the client↔host wire: `_shepherd/pair/*` and `_shepherd/peer/*`.

use serde_json::{Value, json};
use shepherd_core::acpp::{RequestId, Timestamp};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::LinkKind;
use shepherd_core::wire::{
    ClientMessage, ClientRequest, HostMessage, HostNotification, HostResponse, HostResult, Method,
    PairJoin, PairOutcome, PairStart, PairStartOk, PairWait, PeerInfo, PeerList, PeerListOk,
    PeerRemove, Request, Tunnel, decode_client_message, decode_host_message,
    encode_host_notification, encode_request, encode_response,
};

fn request(line: &[u8]) -> Request {
    match decode_client_message(line).unwrap_or_else(|e| panic!("{e:?}")) {
        ClientMessage::Request(r) => r,
        other => panic!("expected a request, got {other:?}"),
    }
}

fn wire_json(line: &[u8]) -> Value {
    serde_json::from_slice(line).unwrap_or_else(|e| panic!("{e}"))
}

fn answered(result: HostResult, method: Method) -> (Value, HostResponse) {
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result: Ok(result),
    });
    let typed = match decode_host_message(&line).unwrap_or_else(|e| panic!("{e}")) {
        HostMessage::Response(r) => r.typed(method).unwrap_or_else(|e| panic!("{e}")),
        other => panic!("expected a response, got {other:?}"),
    };
    (wire_json(&line)["result"].clone(), typed)
}

fn peer(id: &str, name: &str, connected: bool) -> PeerInfo {
    PeerInfo {
        id: MachineId::new(id),
        name: name.into(),
        connected,
        link: None,
        rtt_ms: None,
    }
}

// ---------- links (T32b) ----------

#[test]
fn a_peers_link_and_round_trip_time_are_in_the_list_and_left_out_when_unknown() {
    let linked = PeerInfo {
        link: Some(LinkKind::Relayed),
        rtt_ms: Some(86),
        ..peer("ab12", "laptop", true)
    };
    let ok = PeerListOk {
        peers: vec![linked, peer("cd34", "tower", false)],
    };
    let (wire, typed) = answered(HostResult::PeerList(ok.clone()), Method::PeerList);
    assert_eq!(
        wire,
        json!({ "peers": [
            { "id": "ab12", "name": "laptop", "connected": true, "link": "relay", "rtt_ms": 86 },
            { "id": "cd34", "name": "tower", "connected": false },
        ] })
    );
    assert_eq!(typed.result, Ok(HostResult::PeerList(ok)));
    let direct = PeerInfo {
        link: Some(LinkKind::Direct),
        ..peer("ab12", "laptop", true)
    };
    assert_eq!(
        serde_json::to_value(&direct).unwrap_or_else(|e| panic!("{e}"))["link"],
        "direct"
    );
}

#[test]
fn a_peer_changed_notification_round_trips_under_its_method_name() {
    let info = PeerInfo {
        link: Some(LinkKind::Direct),
        rtt_ms: Some(4),
        ..peer("ab12", "laptop", true)
    };
    let note = HostNotification::PeerChanged(info);
    let line = encode_host_notification(&note);
    assert_eq!(wire_json(&line)["method"], "_shepherd/peer/changed");
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "id": "ab12", "name": "laptop", "connected": true, "link": "direct", "rtt_ms": 4 })
    );
    assert_eq!(
        decode_host_message(&line).unwrap_or_else(|e| panic!("{e}")),
        HostMessage::Notification(note)
    );
}

#[test]
fn each_pairing_and_peer_request_round_trips_under_its_method_name() {
    let requests = [
        (
            ClientRequest::PairStart(PairStart {}),
            "_shepherd/pair/start",
        ),
        (ClientRequest::PairWait(PairWait {}), "_shepherd/pair/wait"),
        (
            ClientRequest::PairJoin(PairJoin {
                ticket: "shepherdabc".into(),
            }),
            "_shepherd/pair/join",
        ),
        (ClientRequest::PeerList(PeerList {}), "_shepherd/peer/list"),
        (
            ClientRequest::PeerRemove(PeerRemove {
                peer: "laptop".into(),
            }),
            "_shepherd/peer/remove",
        ),
    ];
    for (i, (body, method)) in requests.into_iter().enumerate() {
        let req = Request {
            id: RequestId::from(i as i64),
            body,
        };
        let line = encode_request(&req);
        assert_eq!(wire_json(&line)["method"], method);
        assert_eq!(request(&line), req);
    }
}

#[test]
fn join_and_remove_params_are_the_ticket_and_the_peer() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::PairJoin(PairJoin {
            ticket: "shepherdabc".into(),
        }),
    });
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "ticket": "shepherdabc" })
    );
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::PeerRemove(PeerRemove {
            peer: "ab12".into(),
        }),
    });
    assert_eq!(wire_json(&line)["params"], json!({ "peer": "ab12" }));
}

#[test]
fn a_started_pairing_answers_its_ticket_and_when_it_expires() {
    let ok = PairStartOk {
        ticket: "shepherdabc".into(),
        expires: Timestamp::parse("2026-10-29T12:10:00Z").unwrap_or_else(|e| panic!("{e}")),
    };
    let (wire, typed) = answered(HostResult::PairStart(ok.clone()), Method::PairStart);
    assert_eq!(
        wire,
        json!({ "ticket": "shepherdabc", "expires": "2026-10-29T12:10:00Z" })
    );
    assert_eq!(typed.result, Ok(HostResult::PairStart(ok)));
}

#[test]
fn each_way_a_pairing_ends_round_trips() {
    let outcomes = [
        (
            PairOutcome::Paired {
                peer: peer("ab12", "laptop", false),
            },
            json!({ "outcome": "paired", "peer": { "id": "ab12", "name": "laptop", "connected": false } }),
        ),
        (
            PairOutcome::Refused {
                peer: MachineId::new("cd34"),
                name: "stranger".into(),
            },
            json!({ "outcome": "refused", "peer": "cd34", "name": "stranger" }),
        ),
        (PairOutcome::Expired, json!({ "outcome": "expired" })),
        (PairOutcome::Replaced, json!({ "outcome": "replaced" })),
    ];
    for (outcome, expected) in outcomes {
        let (wire, typed) = answered(HostResult::PairWait(outcome.clone()), Method::PairWait);
        assert_eq!(wire, expected);
        assert_eq!(typed.result, Ok(HostResult::PairWait(outcome)));
    }
}

#[test]
fn a_join_and_a_removal_answer_the_peer() {
    let (wire, typed) = answered(
        HostResult::PairJoin(peer("ab12", "desk", true)),
        Method::PairJoin,
    );
    assert_eq!(
        wire,
        json!({ "id": "ab12", "name": "desk", "connected": true })
    );
    assert_eq!(
        typed.result,
        Ok(HostResult::PairJoin(peer("ab12", "desk", true)))
    );
    let (_, typed) = answered(
        HostResult::PeerRemove(peer("ab12", "desk", false)),
        Method::PeerRemove,
    );
    assert_eq!(
        typed.result,
        Ok(HostResult::PeerRemove(peer("ab12", "desk", false)))
    );
}

#[test]
fn the_peer_list_names_each_peer_and_a_peer_without_connected_is_not_connected() {
    let list = PeerListOk {
        peers: vec![peer("ab12", "desk", true), peer("cd34", "laptop", false)],
    };
    let (_, typed) = answered(HostResult::PeerList(list.clone()), Method::PeerList);
    assert_eq!(typed.result, Ok(HostResult::PeerList(list)));
    let parsed: PeerInfo = serde_json::from_value(json!({ "id": "ab12", "name": "desk" }))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!parsed.connected);
}

#[test]
fn a_tunnel_names_the_machine_and_is_answered_with_an_empty_ack() {
    let req = Request {
        id: 1.into(),
        body: ClientRequest::Tunnel(Tunnel {
            machine: MachineId::new("bb"),
        }),
    };
    let line = encode_request(&req);
    assert_eq!(wire_json(&line)["method"], "_shepherd/tunnel");
    assert_eq!(wire_json(&line)["params"], json!({ "machine": "bb" }));
    assert_eq!(request(&line), req);
    let (value, typed) = answered(HostResult::Ack, Method::Tunnel);
    assert_eq!(value, json!({}));
    assert_eq!(typed.result, Ok(HostResult::Ack));
}
