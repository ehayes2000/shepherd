//! What a machine is called (T29): `[machine] name`, else the hostname, else "this machine".

use serde_json::json;
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::config::{Config, RawConfig};
use shepherd_core::host::{Host, HostCommand, HostEvent};
use shepherd_core::machine::{UNNAMED, name};
use shepherd_testkit::builders::HostDriver;

/// What the host appends to its log when it starts under a name it hasn't recorded last.
fn renamed(name: &str) -> HostCommand {
    let frame = Frame::from_value(json!({
        "jsonrpc": "2.0",
        "method": "_shepherd/machine/renamed",
        "params": { "name": name },
    }))
    .unwrap_or_else(|e| panic!("bad frame: {e}"));
    let draft = FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    );
    HostCommand::Append(Box::new(draft))
}

fn start_as(host: &mut HostDriver, name: &str) -> Vec<HostCommand> {
    host.handle(HostEvent::MachineNamed { name: name.into() })
}

#[test]
fn a_first_start_records_the_machine_name() {
    let mut host = HostDriver::new();
    assert_eq!(host.host().machine_name(), None);
    assert_eq!(start_as(&mut host, "studio"), vec![renamed("studio")]);
    assert_eq!(host.host().machine_name(), Some("studio"));
}

#[test]
fn starting_under_the_recorded_name_records_nothing() {
    let mut host = HostDriver::new();
    start_as(&mut host, "studio");
    assert_eq!(start_as(&mut host, "studio"), vec![]);
}

#[test]
fn starting_under_another_name_records_the_rename() {
    let mut host = HostDriver::new();
    start_as(&mut host, "studio");
    assert_eq!(start_as(&mut host, "desk"), vec![renamed("desk")]);
    assert_eq!(host.host().machine_name(), Some("desk"));
}

#[test]
fn the_latest_recorded_name_survives_replay() {
    let mut host = HostDriver::new();
    start_as(&mut host, "studio");
    start_as(&mut host, "desk");
    let mut replayed = Host::new();
    for envelope in host.log() {
        replayed.apply(envelope);
    }
    assert_eq!(replayed.machine_name(), Some("desk"));
}

fn named(machine_name: &str) -> Config {
    let raw: RawConfig = serde_json::from_value(json!({ "machine": { "name": machine_name } }))
        .unwrap_or_else(|e| panic!("bad RawConfig: {e}"));
    Config::resolve(Some(raw), None).unwrap_or_else(|e| panic!("{e}"))
}

fn unnamed() -> Config {
    Config::resolve(None, None).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_config_name_wins_over_the_hostname() {
    assert_eq!(name(&named("studio"), Some("box.local")), "studio");
    assert_eq!(name(&named("studio"), None), "studio");
}

#[test]
fn without_a_config_name_the_hostname_names_the_machine() {
    assert_eq!(name(&unnamed(), Some("box.local")), "box.local");
}

#[test]
fn the_hostname_is_made_display_safe() {
    assert_eq!(
        name(&unnamed(), Some("\u{1b}]0;evil\u{7}box\u{202e}")),
        "]0;evilbox"
    );
}

#[test]
fn a_long_hostname_is_cut_to_64_grapheme_clusters() {
    let host = format!("{}tail", "a".repeat(64));
    assert_eq!(name(&unnamed(), Some(&host)), "a".repeat(64));
}

#[test]
fn without_a_usable_hostname_the_machine_is_this_machine() {
    assert_eq!(UNNAMED, "this machine");
    assert_eq!(name(&unnamed(), None), UNNAMED);
    assert_eq!(name(&unnamed(), Some("")), UNNAMED);
    assert_eq!(name(&unnamed(), Some(" \u{1b}\u{200b} ")), UNNAMED);
}
