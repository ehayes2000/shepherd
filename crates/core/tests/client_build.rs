//! What the client does when its host runs another build (T36a, spike T36's table), through the public
//! `shepherd_core::client` API.

use serde_json::{Value, json};
use shepherd_core::acpp::RequestId;
use shepherd_core::client::{
    Cell, Client, ClientCommand, ClientEvent, Connection, HostOutdated, Intent, MenuItem, Popup,
};
use shepherd_core::client::{UPGRADE_DEADLINE, UPGRADE_RETRY};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    BuildId, ClientRequest, ExeDigest, HostMessage, HostUpgrade, PROTOCOL_VERSION, RawResponse,
    UpgradeAnswer, WireError,
};
use shepherd_testkit::builders::{ClientDriver, requests, ts};

/// The notice for an odd-one-out client.
const ODD_ONE_OUT: &str =
    "this window runs another build than the host: restart it to use the installed one";
/// The notice when client, host and installed binary all differ.
const ALL_DIFFER: &str =
    "this window and the host run different builds, and neither is the installed one";
/// While a stale host updates itself in place, and once it has.
const UPDATING: &str = "host is older — updating…";
const UPDATED: &str = "host updated";
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
fn a_stale_host_is_asked_to_update_itself_and_the_user_is_told() {
    // The client is what's installed now; the host runs what was installed when it started.
    let mut c = client(1);
    let sent = hello(&mut c, Ok(hello_ok(Some(2), Some(1))));
    assert_eq!(
        upgrade_request(&sent).map(|(_, up)| up),
        Some(HostUpgrade { force: false })
    );
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().notice(), Some(UPDATING));
}

// ---------- updating a stale host in place (T36b) ----------

/// The upgrade request among `commands`, with its id.
fn upgrade_request(commands: &[ClientCommand]) -> Option<(RequestId, HostUpgrade)> {
    requests(commands).into_iter().find_map(|r| match &r.body {
        ClientRequest::HostUpgrade(up) => Some((r.id.clone(), *up)),
        _ => None,
    })
}

/// Answers the upgrade request among `sent` with `answer`.
fn answer_upgrade(
    c: &mut ClientDriver,
    sent: &[ClientCommand],
    answer: Result<UpgradeAnswer, WireError>,
) -> Vec<ClientCommand> {
    let (id, _) = upgrade_request(sent).unwrap_or_else(|| panic!("no upgrade request in {sent:?}"));
    let result = answer.map(|a| serde_json::to_value(a).unwrap_or_else(|e| panic!("{e}")));
    c.handle(ClientEvent::Host(HostMessage::Response(RawResponse {
        id,
        result,
    })))
}

/// A client of build 1 that has just asked a stale host (build 2, with build 1 installed) to update. Returns the
/// commands holding the request.
fn asking() -> (ClientDriver, Vec<ClientCommand>) {
    let mut c = client(1);
    let sent = hello(&mut c, Ok(hello_ok(Some(2), Some(1))));
    (c, sent)
}

fn at(text: &str) -> ClientEvent {
    ClientEvent::Tick { at: ts(text) }
}

#[test]
fn an_accepted_update_says_host_updated_once_the_reconnect_finds_the_new_build() {
    let (mut c, sent) = asking();
    assert_eq!(
        answer_upgrade(&mut c, &sent, Ok(UpgradeAnswer::Accepted)),
        []
    );
    assert_eq!(c.client().notice(), Some(UPDATING));

    // The host re-execs: the link drops, and the reconnect's hello finds the installed build.
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    let after = c.answer(&sent, Ok(hello_ok(Some(1), Some(1))));
    assert_eq!(upgrade_request(&after), None);
    assert_eq!(c.client().notice(), Some(UPDATED));
    assert_eq!(c.client().popup(), None);
}

#[test]
fn a_busy_host_is_asked_again_after_the_retry_interval() {
    assert_eq!(UPGRADE_RETRY.as_secs(), 1, "the ticks below assume it");
    let (mut c, sent) = asking();
    answer_upgrade(
        &mut c,
        &sent,
        Ok(UpgradeAnswer::Busy {
            reason: "a hook is running".into(),
        }),
    );
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.handle(at("2026-10-29T12:00:10.000Z")), []);
    let early = c.handle(at("2026-10-29T12:00:10.500Z"));
    assert_eq!(upgrade_request(&early), None);
    let retried = c.handle(at("2026-10-29T12:00:11.000Z"));
    assert_eq!(
        upgrade_request(&retried).map(|(_, up)| up),
        Some(HostUpgrade { force: false })
    );
}

#[test]
fn a_host_busy_past_the_deadline_falls_back_to_the_restart_dialog() {
    let (mut c, mut sent) = asking();
    let busy = || {
        Ok(UpgradeAnswer::Busy {
            reason: "a worktree is being made".into(),
        })
    };
    answer_upgrade(&mut c, &sent, busy());
    c.handle(at("2026-10-29T12:00:00.000Z"));
    let mut second = 0;
    // Keep answering busy, a retry a second, until the client gives up.
    while c.client().popup().is_none() {
        second += 1;
        assert!(second <= 600, "never gave up");
        let tick = format!("2026-10-29T12:{:02}:{:02}.000Z", second / 60, second % 60);
        let retried = c.handle(at(&tick));
        if upgrade_request(&retried).is_some() {
            sent = retried;
            answer_upgrade(&mut c, &sent, busy());
        }
    }
    assert_eq!(i64::from(second), UPGRADE_DEADLINE.as_secs());
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Stale)
    );
}

#[test]
fn an_update_the_host_cannot_do_falls_back_to_the_restart_dialog_which_asks_first() {
    let (mut c, sent) = asking();
    let commands = answer_upgrade(
        &mut c,
        &sent,
        Ok(UpgradeAnswer::Unavailable {
            reason: "overwritten in place? reinstall it".into(),
        }),
    );
    assert!(
        !commands.contains(&ClientCommand::TerminateHost),
        "{commands:?}"
    );
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Stale)
    );
    assert!(
        c.client()
            .notice()
            .is_some_and(|n| n.contains("reinstall it")),
        "{:?}",
        c.client().notice()
    );
}

#[test]
fn a_host_that_does_not_know_upgrade_gets_the_restart_dialog() {
    let (mut c, sent) = asking();
    answer_upgrade(
        &mut c,
        &sent,
        Err(WireError::MethodNotFound("_shepherd/host/upgrade".into())),
    );
    assert_eq!(
        c.client().popup().cloned(),
        restart_dialog(HostOutdated::Stale)
    );
}

#[test]
fn a_host_with_nothing_to_do_ends_the_update_quietly() {
    let (mut c, sent) = asking();
    answer_upgrade(&mut c, &sent, Ok(UpgradeAnswer::NothingToDo));
    assert_eq!((c.client().popup(), c.client().notice()), (None, None));
    let later = c.handle(at("2026-10-29T12:05:00.000Z"));
    assert_eq!(upgrade_request(&later), None);
}

#[test]
fn a_busy_update_ends_when_the_link_drops_and_the_next_hello_decides_afresh() {
    let (mut c, sent) = asking();
    answer_upgrade(
        &mut c,
        &sent,
        Ok(UpgradeAnswer::Busy { reason: "x".into() }),
    );
    c.handle(ClientEvent::Disconnected);
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    c.answer(&sent, Ok(hello_ok(Some(1), Some(1))));
    // Not "host updated": this client never saw its update accepted.
    assert_eq!(c.client().notice(), None);
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
    let (mut c, sent) = asking();
    answer_upgrade(
        &mut c,
        &sent,
        Ok(UpgradeAnswer::Unavailable { reason: "x".into() }),
    );
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

const WARNING: &str = "the host couldn't use its config and runs with the defaults";

/// A client whose host's hello answer carries [`WARNING`].
fn warned() -> ClientDriver {
    let mut answer = hello_ok(Some(1), Some(1));
    answer["warning"] = json!(WARNING);
    answered(Ok(answer))
}

#[test]
fn a_warning_in_the_hello_answer_is_shown_as_a_notice() {
    let c = warned();
    assert_eq!(c.client().notice(), Some(WARNING));
    assert_eq!(c.client().host_warning(), Some(WARNING));
}

#[test]
fn a_host_warning_outlasts_its_notice_and_can_be_shown_again() {
    let mut c = warned();
    c.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    c.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:01:00.000Z"),
    });
    assert_eq!(c.client().notice(), None, "the notice timed out");
    assert_eq!(c.client().host_warning(), Some(WARNING));
    assert_eq!(c.user(Intent::ShowHostWarning), []);
    assert_eq!(c.client().notice(), Some(WARNING));
}

#[test]
fn a_host_warning_lasts_until_a_hello_without_one() {
    let mut c = warned();
    c.handle(ClientEvent::Disconnected);
    assert_eq!(
        c.client().host_warning(),
        Some(WARNING),
        "kept across the outage"
    );
    let sent = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:01.000Z"),
    });
    c.answer(&sent, Ok(hello_ok(Some(1), Some(1))));
    assert_eq!(c.client().host_warning(), None);
    assert_eq!(c.user(Intent::ShowHostWarning), []);
    assert_eq!(c.client().notice(), None);
}
