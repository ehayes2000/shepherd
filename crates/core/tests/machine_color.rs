//! Each machine's color (T62): derived from its id so every client agrees, unless its host's config gives one
//! (`[machine] color`), which its hello carries.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use shepherd_core::client::{ClientCommand, ClientEvent};
use shepherd_core::config::{Config, ConfigError, RawConfig};
use shepherd_core::ids::MachineId;
use shepherd_core::machine::MachineColor;
use shepherd_core::wire::{Hello, HelloOk, PROTOCOL_VERSION, negotiate};
use shepherd_testkit::builders::{ClientDriver, ts};

fn m(id: &str) -> MachineId {
    MachineId::new(id)
}

fn resolve(value: Value) -> Result<Config, ConfigError> {
    let raw: RawConfig =
        serde_json::from_value(value).unwrap_or_else(|e| panic!("bad RawConfig: {e}"));
    Config::resolve(Some(raw), None)
}

// ---------- derived from the id ----------

#[test]
fn the_same_id_always_gets_the_same_color() {
    for id in ["m_test", "m_laptop", "3f9a0c", ""] {
        assert_eq!(MachineColor::of(&m(id)), MachineColor::of(&m(id)), "{id:?}");
    }
}

/// The color is SHA-256 of the id, so it is the same in every build of every client: pinned here.
#[test]
fn the_color_of_an_id_is_pinned_across_builds() {
    assert_eq!(MachineColor::of(&m("m_test")), MachineColor::Cyan);
    assert_eq!(MachineColor::of(&m("m_a")), MachineColor::Magenta);
    assert_eq!(MachineColor::of(&m("m_tower")), MachineColor::Blue);
    assert_eq!(MachineColor::of(&m("m_laptop")), MachineColor::LightCyan);
    assert_eq!(MachineColor::of(&m("m_2")), MachineColor::LightBlue);
}

#[test]
fn ids_spread_over_the_whole_palette() {
    let used: BTreeSet<_> = (0..64)
        .map(|n| MachineColor::of(&m(&format!("m_{n}"))))
        .collect();
    assert_eq!(used.len(), MachineColor::ALL.len(), "{used:?}");
}

#[test]
fn every_color_is_named_by_what_it_parses_from() {
    for color in MachineColor::ALL {
        assert_eq!(MachineColor::parse(color.as_str()), Some(color));
    }
    assert_eq!(
        MachineColor::parse("light-blue"),
        Some(MachineColor::LightBlue)
    );
    assert_eq!(MachineColor::parse("teal"), None);
}

// ---------- [machine] color ----------

#[test]
fn without_a_color_in_the_config_there_is_no_override() {
    assert_eq!(Config::resolve(None, None).unwrap().machine_color(), None);
    let config = resolve(json!({ "machine": { "name": "studio" } })).unwrap();
    assert_eq!(config.machine_color(), None);
}

#[test]
fn the_machine_table_overrides_this_machines_color() {
    let config = resolve(json!({ "machine": { "color": "light-magenta" } })).unwrap();
    assert_eq!(config.machine_color(), Some(MachineColor::LightMagenta));
}

#[test]
fn a_color_outside_the_palette_is_an_error() {
    let result = resolve(json!({ "machine": { "color": "black" } }));
    assert_eq!(
        result,
        Err(ConfigError::UnknownMachineColor("black".to_owned()))
    );
}

// ---------- on the wire ----------

fn hello() -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        machine: m("m_client"),
        cwd: None,
        build: None,
    }
}

#[test]
fn the_hello_carries_the_configs_color_by_name() {
    let config = resolve(json!({ "machine": { "color": "blue" } })).unwrap();
    let ok = negotiate(&hello(), &m("m_host"))
        .unwrap()
        .with_color(&config);
    assert_eq!(ok.color, Some(MachineColor::Blue));
    let value = serde_json::to_value(&ok).unwrap();
    assert_eq!(value["color"], json!("blue"));
}

#[test]
fn without_a_configured_color_the_hello_leaves_it_off_the_wire() {
    let config = Config::resolve(None, None).unwrap();
    let ok = negotiate(&hello(), &m("m_host"))
        .unwrap()
        .with_color(&config);
    let value = serde_json::to_value(&ok).unwrap();
    assert_eq!(value.get("color"), None, "{value}");
}

#[test]
fn a_color_this_client_does_not_know_reads_as_none() {
    for color in [json!("chartreuse"), json!(7), json!({ "rgb": [1, 2, 3] })] {
        let ok: HelloOk = serde_json::from_value(json!({
            "version": PROTOCOL_VERSION,
            "machine": "m_new",
            "color": color,
        }))
        .unwrap_or_else(|e| panic!("{color}: {e}"));
        assert_eq!(ok.color, None);
    }
}

// ---------- what the client draws ----------

/// A client connected to a host whose hello answer is `ok`.
fn connected(ok: Value) -> ClientDriver {
    let mut c = ClientDriver::new();
    let started = c.handle(ClientEvent::Start);
    assert_eq!(started, [ClientCommand::Connect]);
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    c.answer(&hello, Ok(ok));
    c
}

#[test]
fn a_machine_whose_host_gives_no_color_gets_the_one_its_id_derives() {
    let c = connected(json!({ "version": PROTOCOL_VERSION, "machine": "m_test" }));
    let client = c.client();
    assert_eq!(
        client.machine_color(&m("m_test")),
        MachineColor::of(&m("m_test"))
    );
    assert_eq!(
        client.machine_color(&m("m_else")),
        MachineColor::of(&m("m_else"))
    );
}

#[test]
fn this_machines_host_overrides_its_color() {
    let c = connected(
        json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "color": "light-blue" }),
    );
    let client = c.client();
    assert_eq!(client.machine_color(&m("m_test")), MachineColor::LightBlue);
    // Only its own.
    assert_eq!(
        client.machine_color(&m("m_else")),
        MachineColor::of(&m("m_else"))
    );
}
