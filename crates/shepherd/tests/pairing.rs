//! Pairing two machines (T30), end to end through two host runtimes on fakes whose `Peers` ports share one network:
//! `shepherd pair` on one, `shepherd join <ticket>` on the other, then each trusts the other, finds it on restart,
//! and forgets it on `shepherd unpair`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use shepherd_core::acpp::{Envelope, Frame, Seq, ShepherdFrame};
use shepherd_core::config::{RawConfig, RawMachine};
use shepherd_core::ports::{EventLog, LogId};
use shepherd_core::wire::{ClientRequest, PairJoin, PairStart, PairWait, PeerList, PeerRemove};
use shepherd_testkit::fakes::peers::FakePeerNet;
use support::*;

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];

fn named(name: &str) -> RawConfig {
    RawConfig {
        machine: Some(RawMachine {
            name: Some(name.into()),
        }),
        ..shell_default_config()
    }
}

fn host(net: &FakePeerNet, key: [u8; 32], name: &str) -> HostHarness {
    HostHarness::builder()
        .peer_net(net.clone())
        .machine_key(key)
        .config(named(name))
        .start()
}

/// The `peer/*` frames in `log`'s host log, as `(method, params)`.
fn peer_frames(log: &impl EventLog) -> Vec<(String, Value)> {
    log.read(LogId::Host, Seq::ZERO)
        .expect("host log")
        .iter()
        .filter_map(|e: &Envelope| match e.frame() {
            Frame::Shepherd(m)
                if matches!(
                    m.body(),
                    ShepherdFrame::PeerTrusted(_) | ShepherdFrame::PeerRemoved(_)
                ) =>
            {
                let v = serde_json::to_value(e.frame()).expect("frame");
                Some((
                    v["method"].as_str().expect("method").to_owned(),
                    v["params"].clone(),
                ))
            }
            _ => None,
        })
        .collect()
}

/// Runs both hosts until neither has anything left to do: the fake network delivers into both.
fn settle(a: &mut HostHarness, b: &mut HostHarness) {
    for _ in 0..4 {
        a.step();
        b.step();
    }
}

#[test]
fn hosts_that_never_paired_never_go_online() {
    let net = FakePeerNet::new();
    let a = host(&net, KEY_A, "office");
    assert!(!net.is_bound(&a.machine()));
    assert_eq!(peer_frames(a.log()), vec![]);
}

#[test]
fn pairing_trusts_each_machine_on_the_other_and_they_find_each_other_after_a_restart() {
    let net = FakePeerNet::new();
    let mut a = host(&net, KEY_A, "office");
    let mut b = host(&net, KEY_B, "laptop");
    let (id_a, id_b) = (a.machine(), b.machine());

    // `shepherd pair` on A: a ticket, and the pairing waits.
    let mut pair = a.connect();
    let start = pair.request(ClientRequest::PairStart(PairStart {}));
    a.step();
    let started = response(&pair.take(), &start).expect("pairing started");
    let ticket = started["ticket"].as_str().expect("a ticket").to_owned();
    assert_eq!(
        started["expires"], "2026-10-29T12:10:00.002Z",
        "ten minutes after it started"
    );
    assert!(net.is_bound(&id_a) && net.is_pairing(&id_a));
    let wait = pair.request(ClientRequest::PairWait(PairWait {}));
    a.step();
    assert_eq!(pair.take(), vec![], "still waiting");

    // `shepherd join <ticket>` on B.
    let mut join = b.connect();
    let joined = join.request(ClientRequest::PairJoin(PairJoin { ticket }));
    settle(&mut a, &mut b);
    assert_eq!(
        response(&join.take(), &joined),
        Ok(json!({ "id": id_a.as_str(), "name": "office", "connected": false }))
    );
    assert_eq!(
        response(&pair.take(), &wait),
        Ok(
            json!({ "outcome": "paired", "peer": { "id": id_b.as_str(), "name": "laptop", "connected": false } })
        )
    );
    assert_eq!(
        peer_frames(a.log()),
        vec![(
            "_shepherd/peer/trusted".to_owned(),
            json!({ "node": id_b.as_str(), "name": "laptop" })
        )]
    );
    assert_eq!(
        peer_frames(b.log()),
        vec![(
            "_shepherd/peer/trusted".to_owned(),
            json!({ "node": id_a.as_str(), "name": "office" })
        )]
    );
    assert_eq!(net.trusted(&id_a), [id_b.clone()].into());
    assert_eq!(net.trusted(&id_b), [id_a.clone()].into());
    assert!(!net.is_pairing(&id_a), "used up");

    // A restarts: it trusts B, so it binds at once and dials it.
    a.restart();
    settle(&mut a, &mut b);
    assert!(net.is_bound(&id_a));
    let mut client = a.connect();
    let list = client.request(ClientRequest::PeerList(PeerList {}));
    a.step();
    assert_eq!(
        response(&client.take(), &list),
        Ok(json!({ "peers": [{ "id": id_b.as_str(), "name": "laptop", "connected": true }] }))
    );
}

#[test]
fn unpairing_records_the_removal_drops_the_connection_and_goes_offline() {
    let net = FakePeerNet::new();
    let mut a = host(&net, KEY_A, "office");
    let mut b = host(&net, KEY_B, "laptop");
    let mut pair = a.connect();
    let start = pair.request(ClientRequest::PairStart(PairStart {}));
    a.step();
    let ticket = response(&pair.take(), &start).expect("started")["ticket"]
        .as_str()
        .expect("ticket")
        .to_owned();
    b.connect()
        .request(ClientRequest::PairJoin(PairJoin { ticket }));
    settle(&mut a, &mut b);
    a.restart();
    settle(&mut a, &mut b);

    let mut client = a.connect();
    let removed = client.request(ClientRequest::PeerRemove(PeerRemove {
        peer: "laptop".into(),
    }));
    settle(&mut a, &mut b);
    assert_eq!(
        response(&client.take(), &removed),
        Ok(json!({ "id": b.machine().as_str(), "name": "laptop", "connected": false }))
    );
    assert_eq!(
        peer_frames(a.log()).last(),
        Some(&(
            "_shepherd/peer/removed".to_owned(),
            json!({ "node": b.machine().as_str() })
        ))
    );
    assert_eq!(net.trusted(&a.machine()), Default::default());
    assert!(!net.is_bound(&a.machine()), "nothing left to be online for");
    // B still trusts A, but finds it gone.
    let mut client = b.connect();
    let list = client.request(ClientRequest::PeerList(PeerList {}));
    b.step();
    assert_eq!(
        response(&client.take(), &list).expect("list")["peers"][0]["connected"],
        false
    );
}

#[test]
fn a_pairing_nobody_joins_expires_after_ten_minutes_and_the_host_goes_offline() {
    let net = FakePeerNet::new();
    let mut a = host(&net, KEY_A, "office");
    let mut pair = a.connect();
    pair.request(ClientRequest::PairStart(PairStart {}));
    let wait = pair.request(ClientRequest::PairWait(PairWait {}));
    a.step();
    assert!(net.is_bound(&a.machine()));
    a.clock().advance(Duration::from_secs(9 * 60));
    a.step();
    assert!(net.is_pairing(&a.machine()), "not yet");
    a.clock().advance(Duration::from_secs(60));
    a.step();
    assert_eq!(
        response(&pair.take(), &wait),
        Ok(json!({ "outcome": "expired" }))
    );
    assert!(!net.is_pairing(&a.machine()));
    assert!(!net.is_bound(&a.machine()));
}

#[test]
fn a_pairing_whose_client_hangs_up_is_stopped_and_the_host_goes_offline() {
    let net = FakePeerNet::new();
    let mut a = host(&net, KEY_A, "office");
    let mut pair = a.connect();
    let start = pair.request(ClientRequest::PairStart(PairStart {}));
    a.step();
    response(&pair.take(), &start).expect("pairing started");
    pair.request(ClientRequest::PairWait(PairWait {}));
    a.step();
    // Another client coming and going changes nothing.
    a.connect().close();
    a.step();
    assert!(net.is_pairing(&a.machine()));
    pair.close();
    a.step();
    assert!(!net.is_pairing(&a.machine()));
    assert!(!net.is_bound(&a.machine()));
}

#[test]
fn shutting_down_takes_the_host_offline() {
    let net = FakePeerNet::new();
    let mut a = host(&net, KEY_A, "office");
    // Kept open: a pairing whose client went would be stopped.
    let mut pair = a.connect();
    pair.request(ClientRequest::PairStart(PairStart {}));
    a.step();
    assert!(net.is_bound(&a.machine()));
    a.send(shepherd::host::RuntimeEvent::Shutdown);
    assert!(a.runtime().is_finished());
    assert!(!net.is_bound(&a.machine()));
}
