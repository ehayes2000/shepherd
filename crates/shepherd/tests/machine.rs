//! The machine's identity and name on the host runtime (T29), on all-fake ports. See `support` for the harness: its
//! machine key is [`MACHINE_KEY`] and its hostname [`HOSTNAME`].
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use shepherd_core::acpp::{Frame, Seq, ShepherdFrame};
use shepherd_core::config::{RawConfig, RawMachine};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{EventLog, LogId};
use shepherd_core::wire::{ClientRequest, Hello, PROTOCOL_VERSION};
use support::*;

/// Every `machine/renamed` in the host log, by name, in order.
fn renames(h: &HostHarness) -> Vec<String> {
    h.log()
        .read(LogId::Host, Seq::ZERO)
        .unwrap()
        .iter()
        .filter_map(|e| match e.frame() {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::MachineRenamed(r) => Some(r.name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn named(name: &str) -> RawConfig {
    RawConfig {
        machine: Some(RawMachine {
            name: Some(name.to_owned()),
        }),
        ..shell_default_config()
    }
}

/// The host's hello answer, untyped.
fn hello(h: &mut HostHarness) -> serde_json::Value {
    let mut client = h.connect();
    let req = client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: None,
        build: None,
    }));
    h.step();
    response(&client.take(), &req).expect("hello ok")
}

#[test]
fn the_harness_machine_is_its_keys_id() {
    assert_eq!(machine().as_str(), "5e".repeat(32));
}

#[test]
fn every_entry_the_host_appends_is_stamped_with_this_machines_id() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    client.new_session();
    h.step();
    let logs = h.log().logs().unwrap();
    let all: Vec<_> = logs
        .into_iter()
        .flat_map(|id| h.log().read(id, Seq::ZERO).unwrap())
        .collect();
    assert!(all.iter().any(|e| e.session().is_none()), "{all:?}");
    assert!(all.iter().any(|e| e.session().is_some()), "{all:?}");
    assert!(all.iter().all(|e| e.machine() == &machine()), "{all:?}");
}

#[test]
fn a_first_start_records_the_hostname_as_the_machine_name() {
    let h = HostHarness::new();
    assert_eq!(renames(&h), [HOSTNAME]);
    assert_eq!(h.host().machine_name(), Some(HOSTNAME));
}

#[test]
fn a_restart_under_the_same_name_records_nothing_and_a_new_name_is_recorded_once() {
    let mut h = HostHarness::new();
    h.restart();
    assert_eq!(renames(&h), [HOSTNAME]);
    h.restart_with_config(named("studio"));
    assert_eq!(renames(&h), [HOSTNAME, "studio"]);
    h.restart();
    assert_eq!(renames(&h), [HOSTNAME, "studio"]);
}

#[test]
fn the_hello_carries_the_machines_id_and_name() {
    let mut h = HostHarness::new();
    let answer = hello(&mut h);
    assert_eq!(answer["machine"], json!(machine().as_str()));
    assert_eq!(answer["name"], json!(HOSTNAME));
    h.restart_with_config(named("studio"));
    let answer = hello(&mut h);
    assert_eq!(answer["machine"], json!(machine().as_str()));
    assert_eq!(answer["name"], json!("studio"));
}
