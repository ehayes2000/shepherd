//! What the client does when its host runs another build (T36a, spike T36's table), through the public
//! `shepherd_core::client` API.

use serde_json::{Value, json};
use shepherd_core::client::{
    Cell, Client, ClientCommand, ClientEvent, Connection, HostOutdated, Intent, MenuItem, Popup,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{BuildId, ClientRequest, ExeDigest, PROTOCOL_VERSION, WireError};
use shepherd_testkit::builders::{ClientDriver, requests, ts};

/// The notice for an odd-one-out client.
const ODD_ONE_OUT: &str =
    "this window runs another build than the host: restart it to use the installed one";
/// The notice when client, host and installed binary all differ.
const ALL_DIFFER: &str =
    "this window and the host run different builds, and neither is the installed one";
/// What a `-32601` answer says.
const OLDER_HOST: &str = "the shepherd host is older than this client: restart it to use this";

fn build(byte: u8) -> BuildId {
    BuildId {
        version: "0.1.0".into(),
        digest: ExeDigest::from_bytes([byte; 32]),
    }
}

/// A client of build `byte`.
fn client(byte: u8) -> ClientDriver {
    ClientDriver::from_client(Client::new(MachineId::new("m_client")).with_build(build(byte)))
}

/// A hello answer from a host running build `running` with build `installed` at its exe path. `None` leaves the
/// field off, as an older host does.
fn hello_ok(running: Option<u8>, installed: Option<u8>) -> Value {
    let mut answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test" });
    if let Some(b) = running {
        answer["build"] = serde_json::to_value(build(b)).unwrap_or_else(|e| panic!("{e}"));
    }
    if let Some(b) = installed {
        answer["installed"] =
            serde_json::to_value(ExeDigest::from_bytes([b; 32])).unwrap_or_else(|e| panic!("{e}"));
    }
    answer
}

/// Starts `c`, connects it, and answers its hello with `answer`. Returns the commands the answer produced.
fn hello(c: &mut ClientDriver, answer: Result<Value, WireError>) -> Vec<ClientCommand> {
    assert_eq!(c.handle(ClientEvent::Start), [ClientCommand::Connect]);
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    c.answer(&sent, answer)
}

/// A client of build 1 whose hello `answer` was just handled.
fn answered(answer: Result<Value, WireError>) -> ClientDriver {
    let mut c = client(1);
    hello(&mut c, answer);
    c
}

fn restart_dialog(why: HostOutdated) -> Option<Popup> {
    Some(Popup::RestartHost { why })
}

#[test]
fn the_hello_carries_the_clients_build() {
    let mut c = client(1);
    c.handle(ClientEvent::Start);
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    let [request] = requests(&sent)[..] else {
        panic!("expected one request in {sent:?}");
    };
    let ClientRequest::Hello(hello) = &request.body else {
        panic!("not a hello");
    };
    assert_eq!(hello.build, Some(build(1)));
}

// ---------- the rows of spike T36's table ----------

#[test]
fn a_host_of_the_same_build_shows_nothing() {
    let c = answered(Ok(hello_ok(Some(1), Some(1))));
    assert_eq!((c.client().popup(), c.client().notice()), (None, None));
    assert_eq!(c.client().connection(), &Connection::Ready);
}

#[test]
fn a_host_without_a_build_offers_to_restart_it() {
    let c = answered(Ok(hello_ok(None, None)));
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Older)
    );
    assert_eq!(c.client().connection(), &Connection::Ready);
}

#[test]
fn a_version_mismatch_offers_to_restart_the_host_and_does_not_quit() {
    let mut c = client(1);
    let commands = hello(
        &mut c,
        Err(WireError::VersionMismatch { host: 2, client: 1 }),
    );
    assert!(!commands.contains(&ClientCommand::Quit), "{commands:?}");
    assert_eq!(c.client().fatal(), None);
    assert_eq!(c.client().connection(), &Connection::Incompatible);
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Older)
    );
}

#[test]
fn a_stale_host_offers_to_restart_it() {
    // The client is what's installed now; the host runs what was installed when it started.
    let c = answered(Ok(hello_ok(Some(2), Some(1))));
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Stale)
    );
}

#[test]
fn a_client_of_another_build_than_the_installed_host_is_told_so_without_a_dialog() {
    let c = answered(Ok(hello_ok(Some(2), Some(2))));
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().notice(), Some(ODD_ONE_OUT));
}

#[test]
fn when_client_host_and_installed_all_differ_only_a_notice_shows() {
    let c = answered(Ok(hello_ok(Some(2), Some(3))));
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().notice(), Some(ALL_DIFFER));
}

#[test]
fn a_client_that_does_not_know_its_own_build_compares_nothing() {
    let mut c = ClientDriver::new();
    hello(&mut c, Ok(hello_ok(None, None)));
    assert_eq!((c.client().popup(), c.client().notice()), (None, None));
}

// ---------- the restart dialog ----------

#[test]
fn restart_host_terminates_the_host_and_closes_the_dialog() {
    let mut c = answered(Ok(hello_ok(None, None)));
    assert_eq!(c.user(Intent::RestartHost), [ClientCommand::TerminateHost]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn not_now_closes_the_dialog_and_leaves_the_host_running() {
    let mut c = answered(Ok(hello_ok(None, None)));
    assert_eq!(c.user(Intent::Dismiss), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn restart_host_without_the_dialog_does_nothing() {
    let mut c = answered(Ok(hello_ok(Some(1), Some(1))));
    assert_eq!(c.user(Intent::RestartHost), []);
}

#[test]
fn confirming_the_restart_dialog_with_a_key_does_not_restart() {
    let mut c = answered(Ok(hello_ok(None, None)));
    assert_eq!(c.user(Intent::Confirm), []);
}

#[test]
fn after_a_restart_the_dropped_link_reconnects_and_relaunches_the_host() {
    let mut c = answered(Err(WireError::VersionMismatch { host: 2, client: 1 }));
    c.user(Intent::RestartHost);
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let failed = c.handle(ClientEvent::ConnectFailed {
        at: ts("2026-10-29T12:00:01.000Z"),
        error: "connection refused".into(),
    });
    assert_eq!(failed, [ClientCommand::LaunchHost]);
}

#[test]
fn the_restart_dialog_goes_when_the_link_drops_and_a_matching_host_answers() {
    let mut c = answered(Ok(hello_ok(Some(2), Some(1))));
    assert!(matches!(
        c.client().popup(),
        Some(Popup::RestartHost { .. })
    ));
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    c.answer(&sent, Ok(hello_ok(Some(1), Some(1))));
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.user(Intent::RestartHost), []);
}

#[test]
fn the_restart_dialog_goes_as_soon_as_the_link_drops() {
    let mut c = answered(Ok(hello_ok(None, None)));
    c.handle(ClientEvent::Disconnected);
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.user(Intent::RestartHost), []);
}

#[test]
fn after_not_now_on_an_incompatible_host_the_restart_dialog_can_be_reopened() {
    let mut c = answered(Err(WireError::VersionMismatch { host: 2, client: 1 }));
    c.user(Intent::Dismiss);
    assert_eq!(c.client().restart_offer(), Some(HostOutdated::Older));
    assert_eq!(c.user(Intent::OfferRestart), []);
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Older)
    );
}

#[test]
fn a_method_the_host_does_not_know_offers_a_restart() {
    let mut c = answered(Ok(hello_ok(Some(1), Some(1))));
    assert_eq!(c.client().restart_offer(), None);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/src/app".into()));
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(WireError::MethodNotFound(
            "_shepherd/workspace/select".into(),
        )),
    );
    assert_eq!(c.client().restart_offer(), Some(HostOutdated::Older));
    c.user(Intent::Dismiss);
    c.user(Intent::OfferRestart);
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Older)
    );
}

#[test]
fn a_host_of_the_same_build_offers_no_restart() {
    let mut c = answered(Ok(hello_ok(Some(1), Some(1))));
    c.user(Intent::OfferRestart);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn the_restart_offer_goes_with_the_host() {
    let mut c = answered(Ok(hello_ok(None, None)));
    c.user(Intent::Dismiss);
    c.handle(ClientEvent::Disconnected);
    assert_eq!(c.client().restart_offer(), None);
    c.user(Intent::OfferRestart);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn a_restart_that_fails_says_why() {
    let mut c = answered(Ok(hello_ok(None, None)));
    c.user(Intent::RestartHost);
    c.handle(ClientEvent::TerminateFailed {
        error: "permission denied".into(),
    });
    assert_eq!(
        c.client().notice(),
        Some("could not restart the shepherd host: permission denied")
    );
}

#[test]
fn the_sessions_a_restart_stops_are_counted_once_the_catch_up_is_in() {
    let mut c = client(1);
    let subscribe = hello(&mut c, Ok(hello_ok(None, None)));
    c.host().new_session("s1");
    c.host().new_session("s2");
    c.host().new_session("s3");
    c.host().pty_exited("s3", Some(0));
    c.sync();
    assert_eq!(c.client().sessions_to_stop(), None, "still catching up");
    c.answer(&subscribe, Ok(json!({})));
    assert_eq!(c.client().sessions_to_stop(), Some(2));
}

#[test]
fn an_incompatible_host_has_no_sessions_to_count() {
    let c = answered(Err(WireError::VersionMismatch { host: 2, client: 1 }));
    assert_eq!(c.client().sessions_to_stop(), None);
}

// ---------- -32601 ----------

#[test]
fn a_method_the_host_does_not_know_says_the_host_is_older() {
    let mut c = client(1);
    let subscribe = hello(&mut c, Ok(hello_ok(Some(1), Some(1))));
    c.answer(&subscribe, Ok(json!({})));
    c.host().new_session("s1");
    c.sync();
    c.user(Intent::OpenMenu {
        session: SessionId::new("s1"),
        at: Cell { col: 0, row: 0 },
    });
    c.user(Intent::MenuChoose(MenuItem::Rename));
    let sent = c.user(Intent::Confirm);
    assert!(matches!(
        requests(&sent)[..],
        [r] if matches!(r.body, ClientRequest::SessionRename(_))
    ));
    c.answer(
        &sent,
        Err(WireError::MethodNotFound("_shepherd/session/rename".into())),
    );
    assert_eq!(c.client().notice(), Some(OLDER_HOST));
}

#[test]
fn a_workspace_switch_the_host_does_not_know_says_the_host_is_older_in_the_switcher() {
    let mut c = client(1);
    hello(&mut c, Ok(hello_ok(Some(1), Some(1))));
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/src/app".into()));
    let sent = c.user(Intent::Confirm);
    c.answer(
        &sent,
        Err(WireError::MethodNotFound(
            "_shepherd/workspace/select".into(),
        )),
    );
    assert_eq!(
        c.client().popup().cloned(),
        Some(Popup::Workspace {
            path: "/src/app".into(),
            error: Some(OLDER_HOST.into()),
        })
    );
}
