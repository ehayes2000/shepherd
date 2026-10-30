//! The client's view state and decisions, through the public `shepherd_core::client` API.

use std::path::{Path, PathBuf};

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind, Timestamp};
use shepherd_core::client::{
    Cell, Client, ClientCommand, ClientEvent, Connection, Focus, HELLO_TIMEOUT, Intent,
    MAX_CONNECT_ATTEMPTS, MenuEntry, MenuItem, NOTICE_TIMEOUT, Popup,
};
use shepherd_core::config::AgentKind;
use shepherd_core::host::ResumeUnavailable;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::WorktreeError;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::session::Activity;
use shepherd_core::wire::{
    ClientMessage, ClientNotification, ClientRequest, InputKind, PROTOCOL_VERSION, SessionClose,
    SessionInput, SessionNew, SessionRename, SessionResize, SessionResume, Subscribe, ViewSize,
    WireError, WorkspaceSelect,
};
use shepherd_testkit::builders::{
    ClientDriver, claude_session_spec, log_entry, notifications, requests, ts,
};

fn connected() -> ClientEvent {
    connected_at(0)
}

fn connected_at(ms: i64) -> ClientEvent {
    ClientEvent::Connected { at: at(ms) }
}

fn hello_ok() -> serde_json::Value {
    json!({ "version": PROTOCOL_VERSION, "machine": "m_test" })
}

/// Drops the link, reconnects, answers the hello, and returns the subscribe the client sends.
fn reconnect(c: &mut ClientDriver) -> Subscribe {
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let hello = c.handle(connected());
    let sent = c.answer(&hello, Ok(hello_ok()));
    match &bodies(&sent)[..] {
        [ClientRequest::Subscribe(sub)] => sub.clone(),
        _ => panic!("expected a subscribe in {sent:?}"),
    }
}

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn ids(c: &ClientDriver) -> Vec<String> {
    c.client()
        .sessions()
        .iter()
        .map(|s| s.id.to_string())
        .collect()
}

fn selected(c: &ClientDriver) -> Option<String> {
    c.client().selected().map(|s| s.to_string())
}

fn input(session: &str, data: &[u8]) -> ClientNotification {
    ClientNotification::SessionInput(SessionInput {
        session: sid(session),
        data: data.to_vec(),
        kind: InputKind::Keys,
    })
}

fn resize(session: &str, cols: u16, rows: u16) -> ClientNotification {
    ClientNotification::SessionResize(SessionResize {
        session: sid(session),
        cols,
        rows,
    })
}

fn bodies(commands: &[ClientCommand]) -> Vec<ClientRequest> {
    requests(commands)
        .into_iter()
        .map(|r| r.body.clone())
        .collect()
}

// ---------- session list ----------

#[test]
fn sessions_are_listed_newest_first_from_log_entries() {
    let c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    assert_eq!(ids(&c), ["s3", "s2", "s1"]);
    assert_eq!(c.client().sessions()[0].name, "shell-3");
}

#[test]
fn a_renamed_session_shows_its_new_name() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().rename("s1", "api");
    c.sync();
    assert_eq!(c.client().sessions()[0].name, "api");
}

#[test]
fn a_closed_session_leaves_the_list() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.host().close("s2");
    c.sync();
    assert_eq!(ids(&c), ["s1"]);
}

#[test]
fn an_exited_session_stays_listed_as_exited() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().pty_exited("s1", Some(0));
    c.sync();
    let s = &c.client().sessions()[0];
    assert!(s.exited);
    assert_eq!(s.activity, Activity::Exited { code: Some(0) });
}

#[test]
fn a_replayed_older_entry_does_not_move_the_resume_point_back() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().rename("s1", "api");
    c.sync();
    // A resubscribe that overlaps what the client already has sends `created` (seq 1) again.
    let created = c.host().log()[0].clone();
    c.handle(log_entry(created));
    let from = reconnect(&mut c).from;
    assert_eq!(from.sessions.get(&sid("s1")).map(|s| s.get()), Some(2));
}

#[test]
fn a_replayed_older_entry_is_not_folded_again() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    let prompt = json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
        "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": "hi" }] } });
    let prompt = c.host().append(Some("s1"), hook.clone(), prompt);
    c.host().append(
        Some("s1"),
        hook,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    c.sync();
    assert_eq!(c.client().sessions()[0].activity, Activity::Idle);
    // Folding the old prompt again would start a turn that never ends.
    c.handle(log_entry(prompt));
    assert_eq!(c.client().sessions()[0].activity, Activity::Idle);
}

#[test]
fn session_activity_comes_from_the_session_fold() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(
        Some("s1"),
        hook,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    c.sync();
    assert_eq!(c.client().sessions()[0].activity, Activity::Idle);
}

#[test]
fn an_idle_session_wants_attention_until_it_is_selected() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(
        Some("s1"),
        hook,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    c.sync();
    let s1 = |c: &ClientDriver| c.client().sessions()[1].attention;
    assert!(s1(&c), "idle while not selected");
    c.user(Intent::NextSession);
    assert!(!s1(&c), "seen once selected");
}

// ---------- attention ----------

fn agent_frame(c: &mut ClientDriver, session: &str, frame: serde_json::Value) {
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    c.host().append(Some(session), hook, frame);
}

fn ask_permission(c: &mut ClientDriver, session: &str, id: i64) {
    agent_frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1", "title": "rm -rf build" },
                            "options": [] } }),
    );
}

fn go_idle(c: &mut ClientDriver, session: &str) {
    agent_frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
}

fn start_turn(c: &mut ClientDriver, session: &str, id: i64) {
    agent_frame(
        c,
        session,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": "go" }] } }),
    );
}

fn attention(c: &ClientDriver, session: &str) -> bool {
    c.client()
        .sessions()
        .iter()
        .find(|s| s.id == sid(session))
        .unwrap_or_else(|| panic!("{session} isn't listed"))
        .attention
}

/// The alerts among `commands`: `(session, message)`.
fn alerts(commands: &[ClientCommand]) -> Vec<(String, String)> {
    commands
        .iter()
        .filter_map(|c| match c {
            ClientCommand::Alert { session, message } => {
                Some((session.to_string(), message.clone()))
            }
            _ => None,
        })
        .collect()
}

/// A live client with sessions `s1` and `s2` (selected), whose host has `[notify] bell = bell`.
fn live(bell: bool) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_live_with_bell(bell);
    c.host().new_session("s1");
    c.host().new_session("s2");
    c.sync();
    c
}

#[test]
fn an_unselected_session_asking_permission_wants_attention_until_it_is_selected() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    start_turn(&mut c, "s1", 1);
    ask_permission(&mut c, "s1", 7);
    c.sync();
    assert!(attention(&c, "s1"), "asked while not selected");
    c.user(Intent::SelectSession(sid("s1")));
    assert!(!attention(&c, "s1"), "seen once selected");
}

#[test]
fn a_seen_permission_request_does_not_want_attention_again_after_selecting_away() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    start_turn(&mut c, "s1", 1);
    ask_permission(&mut c, "s1", 7);
    c.sync();
    c.user(Intent::SelectSession(sid("s1")));
    c.user(Intent::SelectSession(sid("s2")));
    assert!(!attention(&c, "s1"));
}

#[test]
fn the_selected_session_asking_permission_is_not_marked() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    start_turn(&mut c, "s2", 1);
    ask_permission(&mut c, "s2", 7);
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    assert!(!attention(&c, "s2"));
}

#[test]
fn a_session_going_idle_again_after_a_seen_idle_wants_attention_again() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    go_idle(&mut c, "s1");
    c.sync();
    c.user(Intent::SelectSession(sid("s1")));
    c.user(Intent::SelectSession(sid("s2")));
    start_turn(&mut c, "s1", 1);
    go_idle(&mut c, "s1");
    c.sync();
    assert!(attention(&c, "s1"));
}

#[test]
fn with_the_bell_on_a_session_newly_wanting_attention_alerts() {
    let mut c = live(true);
    assert!(c.client().bell());
    start_turn(&mut c, "s1", 1);
    ask_permission(&mut c, "s1", 7);
    let sent = c.sync();
    assert_eq!(
        alerts(&sent),
        [(
            "s1".into(),
            "shepherd: shell-1 needs permission: rm -rf build".into()
        )]
    );
}

#[test]
fn an_alert_starts_with_fixed_text_whatever_the_session_is_called() {
    // A name like an OSC 9 subcommand (`4;` progress, `9;` cwd in ConEmu-style terminals) must not lead the message.
    let mut c = live(true);
    c.host().rename("s1", "4;3;50");
    c.sync();
    go_idle(&mut c, "s1");
    let sent = c.sync();
    assert_eq!(
        alerts(&sent),
        [("s1".into(), "shepherd: 4;3;50 is idle".into())]
    );
}

#[test]
fn an_idle_session_alerts_that_it_is_done() {
    let mut c = live(true);
    go_idle(&mut c, "s1");
    let sent = c.sync();
    assert_eq!(
        alerts(&sent),
        [("s1".into(), "shepherd: shell-1 is idle".into())]
    );
}

#[test]
fn a_session_alerts_once_per_wait() {
    let mut c = live(true);
    go_idle(&mut c, "s1");
    c.sync();
    // The agent says it's idle again: the same wait, still unseen.
    go_idle(&mut c, "s1");
    assert_eq!(alerts(&c.sync()), []);
}

#[test]
fn with_the_bell_off_nothing_alerts() {
    let mut c = live(false);
    assert!(!c.client().bell());
    go_idle(&mut c, "s1");
    assert_eq!(alerts(&c.sync()), []);
    assert!(attention(&c, "s1"), "still marked");
}

#[test]
fn a_host_that_says_nothing_about_notify_leaves_the_bell_off() {
    let c = ClientDriver::with_sessions(&["s1"]);
    assert!(!c.client().bell());
}

#[test]
fn the_selected_session_does_not_alert() {
    let mut c = live(true);
    start_turn(&mut c, "s2", 1);
    ask_permission(&mut c, "s2", 7);
    assert_eq!(alerts(&c.sync()), []);
}

#[test]
fn the_catch_up_after_connecting_does_not_alert() {
    let mut c = ClientDriver::new();
    c.host().new_session("s1");
    c.host().new_session("s2");
    go_idle(&mut c, "s1");
    let answer =
        json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "notify": { "bell": true } });
    c.handle(ClientEvent::Start);
    let hello = c.handle(connected());
    let subscribe = c.answer(&hello, Ok(answer));
    // The host replays the log, then answers the subscribe.
    let replayed = c.sync();
    assert_eq!(alerts(&replayed), []);
    assert!(attention(&c, "s1"), "marked, just not alerted");
    assert_eq!(alerts(&c.answer(&subscribe, Ok(json!({})))), []);
    // Live from here.
    start_turn(&mut c, "s1", 1);
    go_idle(&mut c, "s1");
    assert_eq!(
        alerts(&c.sync()),
        [("s1".into(), "shepherd: shell-1 is idle".into())]
    );
}

// ---------- attention follows window focus ----------

#[test]
fn while_the_window_is_unfocused_the_selected_session_going_idle_is_marked() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::WindowFocus(false));
    go_idle(&mut c, "s2");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    assert!(attention(&c, "s2"));
}

#[test]
fn while_the_window_is_unfocused_the_selected_session_asking_permission_alerts() {
    let mut c = live(true);
    c.user(Intent::WindowFocus(false));
    start_turn(&mut c, "s2", 1);
    ask_permission(&mut c, "s2", 7);
    assert_eq!(
        alerts(&c.sync()),
        [(
            "s2".into(),
            "shepherd: shell-2 needs permission: rm -rf build".into()
        )]
    );
}

#[test]
fn the_selected_sessions_mark_clears_when_the_window_regains_focus() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::WindowFocus(false));
    go_idle(&mut c, "s2");
    c.sync();
    c.user(Intent::WindowFocus(true));
    assert!(!attention(&c, "s2"));
}

#[test]
fn regaining_focus_leaves_unselected_sessions_marked() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::WindowFocus(false));
    go_idle(&mut c, "s1");
    c.sync();
    c.user(Intent::WindowFocus(true));
    assert!(attention(&c, "s1"));
}

#[test]
fn after_regaining_focus_the_selected_session_is_seen_again() {
    let mut c = live(true);
    c.user(Intent::WindowFocus(false));
    c.user(Intent::WindowFocus(true));
    go_idle(&mut c, "s2");
    assert_eq!(alerts(&c.sync()), []);
    assert!(!attention(&c, "s2"));
}

#[test]
fn a_wait_seen_before_the_window_lost_focus_does_not_alert_again() {
    let mut c = live(true);
    go_idle(&mut c, "s2");
    c.sync();
    c.user(Intent::WindowFocus(false));
    // The agent repeats it's idle: the same wait, already seen.
    go_idle(&mut c, "s2");
    assert_eq!(alerts(&c.sync()), []);
    assert!(!attention(&c, "s2"));
}

// ---------- selection ----------

#[test]
fn the_newest_session_is_selected_when_the_list_first_fills() {
    let c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn nothing_is_selected_with_no_sessions() {
    let c = ClientDriver::with_sessions(&[]);
    assert_eq!(selected(&c), None);
}

#[test]
fn a_new_session_from_the_host_keeps_the_selection() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::NextSession);
    c.host().new_session("s3");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s1"));
}

#[test]
fn next_session_moves_down_and_wraps_to_the_top() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    let mut seen = Vec::new();
    for _ in 0..3 {
        c.user(Intent::NextSession);
        seen.push(selected(&c).unwrap_or_default());
    }
    assert_eq!(seen, ["s2", "s1", "s3"]);
}

#[test]
fn previous_session_moves_up_and_wraps_to_the_bottom() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    let mut seen = Vec::new();
    for _ in 0..3 {
        c.user(Intent::PrevSession);
        seen.push(selected(&c).unwrap_or_default());
    }
    assert_eq!(seen, ["s1", "s2", "s3"]);
}

#[test]
fn closing_the_selected_session_selects_the_one_below_it() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    c.user(Intent::NextSession); // s2, in the middle
    c.host().close("s2");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s1"));
}

#[test]
fn closing_the_selected_bottom_session_selects_the_one_above_it() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    c.user(Intent::PrevSession); // s1, at the bottom
    c.host().close("s1");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn closing_another_session_keeps_the_selection() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    c.user(Intent::PrevSession); // s1
    c.host().close("s3");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s1"));
}

#[test]
fn closing_the_only_session_selects_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().close("s1");
    c.sync();
    assert_eq!(selected(&c), None);
}

#[test]
fn a_session_the_user_created_is_selected_once_the_host_answers() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::PrevSession);
    let sent = c.user(Intent::NewSession);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionNew(Default::default())]
    );
    // The host logs the session before it answers.
    c.host().new_session("s2");
    c.sync();
    c.answer(&sent, Ok(json!({ "session": "s2" })));
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn a_created_session_answered_before_its_log_entry_is_selected_when_it_arrives() {
    let mut c = ClientDriver::with_sessions(&["s1", "s0"]);
    // The user's pick, so the selection no longer follows the top of the list.
    c.user(Intent::PrevSession);
    assert_eq!(selected(&c).as_deref(), Some("s1"));
    let sent = c.user(Intent::NewSession);
    c.answer(&sent, Ok(json!({ "session": "s2" })));
    c.host().new_session("s3");
    c.host().new_session("s2");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn a_new_session_asks_to_spawn_at_the_views_size() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    let sent = c.user(Intent::NewSession);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionNew(SessionNew {
            size: Some(ViewSize {
                cols: 100,
                rows: 30
            }),
            ..SessionNew::default()
        })]
    );
}

#[test]
fn a_created_session_selected_once_the_host_answers_is_resized_to_the_view() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    c.user(Intent::PrevSession);
    let sent = c.user(Intent::NewSession);
    c.host().new_session("s2");
    c.sync();
    let answered = c.answer(&sent, Ok(json!({ "session": "s2" })));
    assert_eq!(notifications(&answered), [&resize("s2", 90, 20)]);
}

#[test]
fn a_created_session_selected_when_its_log_entry_arrives_is_resized_to_the_view() {
    let mut c = ClientDriver::with_sessions(&["s1", "s0"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    c.user(Intent::PrevSession);
    let sent = c.user(Intent::NewSession);
    c.answer(&sent, Ok(json!({ "session": "s2" })));
    c.host().new_session("s2");
    let synced = c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    assert_eq!(notifications(&synced), [&resize("s2", 90, 20)]);
}

#[test]
fn a_new_session_the_selection_follows_to_is_resized_to_the_view() {
    // No pick yet: the selection follows the top of the list, where the newest session is.
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    c.host().new_session("s2");
    let synced = c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    assert_eq!(notifications(&synced), [&resize("s2", 90, 20)]);
}

// ---------- input and resize ----------

#[test]
fn keystrokes_in_terminal_focus_go_to_the_selected_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(c.client().focus(), Focus::Terminal);
    let sent = c.user(Intent::keys(b"ls\r".to_vec()));
    assert_eq!(notifications(&sent), [&input("s2", b"ls\r")]);
}

#[test]
fn input_goes_to_the_session_tagged_with_what_made_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let sent = c.user(Intent::Input(InputKind::Focus, b"\x1b[I".to_vec()));
    let expected = ClientNotification::SessionInput(SessionInput {
        session: sid("s1"),
        data: b"\x1b[I".to_vec(),
        kind: InputKind::Focus,
    });
    assert_eq!(notifications(&sent), [&expected]);
}

#[test]
fn keystrokes_in_command_focus_are_not_sent() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::SetFocus(Focus::Command));
    assert_eq!(c.client().focus(), Focus::Command);
    assert_eq!(c.user(Intent::keys(b"x".to_vec())), []);
}

#[test]
fn keystrokes_with_no_session_are_dropped() {
    let mut c = ClientDriver::with_sessions(&[]);
    assert_eq!(c.user(Intent::keys(b"x".to_vec())), []);
}

#[test]
fn keystrokes_while_a_popup_is_open_are_not_sent() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::ShowHelp);
    assert_eq!(c.user(Intent::keys(b"x".to_vec())), []);
}

#[test]
fn keystrokes_before_connecting_are_dropped() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    assert_eq!(c.user(Intent::keys(b"x".to_vec())), []);
}

#[test]
fn a_resize_becomes_a_resize_of_the_selected_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    let sent = c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    assert_eq!(notifications(&sent), [&resize("s2", 100, 30)]);
    assert_eq!(
        c.client().size(),
        Some(PtySize {
            cols: 100,
            rows: 30
        })
    );
}

#[test]
fn switching_sessions_resizes_the_newly_selected_one_to_the_view() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    let sent = c.user(Intent::NextSession);
    assert_eq!(notifications(&sent), [&resize("s1", 90, 20)]);
}

// ---------- clicking and scrolling the sidebar ----------

#[test]
fn selecting_a_listed_session_selects_it_and_resizes_it_to_the_view() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2", "s3"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    let sent = c.user(Intent::SelectSession(sid("s1")));
    assert_eq!(selected(&c).as_deref(), Some("s1"));
    assert_eq!(notifications(&sent), [&resize("s1", 90, 20)]);
}

#[test]
fn selecting_the_selected_session_again_sends_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::Resize(PtySize { cols: 90, rows: 20 }));
    assert_eq!(c.user(Intent::SelectSession(sid("s2"))), []);
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn selecting_a_session_that_is_not_listed_does_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(c.user(Intent::SelectSession(sid("gone"))), []);
    assert_eq!(selected(&c).as_deref(), Some("s2"));
}

#[test]
fn a_selected_session_stays_selected_when_a_newer_one_is_listed() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::SelectSession(sid("s1")));
    c.host().new_session("s3");
    c.sync();
    assert_eq!(selected(&c).as_deref(), Some("s1"));
}

/// Ten sessions, newest (`s10`) first and selected, in a sidebar that shows four at a time.
fn ten_in_four() -> ClientDriver {
    let ids: Vec<String> = (1..=10).map(|i| format!("s{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut c = ClientDriver::with_sessions(&ids);
    c.user(Intent::SidebarRows(4));
    c
}

#[test]
fn the_sidebar_starts_at_the_top() {
    assert_eq!(ten_in_four().client().sidebar_scroll(), 0);
}

#[test]
fn scrolling_the_sidebar_moves_its_first_row_and_stops_at_either_end() {
    let mut c = ten_in_four();
    c.user(Intent::ScrollSidebar(3));
    assert_eq!(c.client().sidebar_scroll(), 3);
    // Ten sessions, four shown: the last six rows are as far down as it goes.
    c.user(Intent::ScrollSidebar(100));
    assert_eq!(c.client().sidebar_scroll(), 6);
    c.user(Intent::ScrollSidebar(-2));
    assert_eq!(c.client().sidebar_scroll(), 4);
    c.user(Intent::ScrollSidebar(-100));
    assert_eq!(c.client().sidebar_scroll(), 0);
}

#[test]
fn scrolling_the_sidebar_leaves_the_selection_alone_even_out_of_view() {
    let mut c = ten_in_four();
    assert_eq!(c.user(Intent::ScrollSidebar(5)), []);
    assert_eq!(selected(&c).as_deref(), Some("s10"));
    assert_eq!(c.client().sidebar_scroll(), 5);
}

#[test]
fn selecting_a_session_below_the_fold_scrolls_it_into_view() {
    let mut c = ten_in_four();
    // Wraps to the oldest, at the bottom: index 9 is the last of rows 6..10.
    c.user(Intent::PrevSession);
    assert_eq!(c.client().sidebar_scroll(), 6);
    // And back to the top.
    c.user(Intent::NextSession);
    assert_eq!(c.client().sidebar_scroll(), 0);
}

#[test]
fn selecting_a_session_above_the_fold_scrolls_it_into_view() {
    let mut c = ten_in_four();
    c.user(Intent::ScrollSidebar(6));
    c.user(Intent::SelectSession(sid("s9"))); // index 1
    assert_eq!(c.client().sidebar_scroll(), 1);
}

#[test]
fn selecting_a_session_in_view_after_scrolling_does_not_move_the_list() {
    let mut c = ten_in_four();
    c.user(Intent::ScrollSidebar(5));
    c.user(Intent::SelectSession(sid("s3"))); // index 7, shown in rows 5..9
    assert_eq!(c.client().sidebar_scroll(), 5);
}

#[test]
fn a_shorter_sidebar_keeps_the_selection_in_view() {
    let mut c = ten_in_four();
    c.user(Intent::PrevSession); // index 9, rows 6..10
    c.user(Intent::SidebarRows(2));
    assert_eq!(c.client().sidebar_scroll(), 8);
}

#[test]
fn the_scroll_never_goes_past_the_end_of_a_shrinking_list() {
    let mut c = ten_in_four();
    c.user(Intent::ScrollSidebar(6));
    for id in ["s1", "s2", "s3", "s4", "s5"] {
        c.host().close(id);
    }
    c.sync();
    // Five left, four shown.
    assert_eq!(c.client().sidebar_scroll(), 1);
}

// ---------- focus and popups ----------

#[test]
fn help_opens_and_dismiss_closes_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::ShowHelp);
    assert_eq!(c.client().popup(), Some(&Popup::Help));
    c.user(Intent::Dismiss);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn renaming_edits_the_current_name_then_sends_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginRename);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Rename {
            session: sid("s1"),
            name: "shell-1".into()
        })
    );
    c.user(Intent::EditRename("api".into()));
    let sent = c.user(Intent::Confirm);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionRename(SessionRename {
            session: sid("s1"),
            name: "api".into()
        })]
    );
    assert_eq!(c.client().popup(), None);
}

#[test]
fn closing_asks_for_confirmation_first() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.user(Intent::BeginClose), []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose { session: sid("s1") })
    );
    let sent = c.user(Intent::Confirm);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionClose(SessionClose {
            session: sid("s1"),
            worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        })]
    );
}

#[test]
fn dismissing_a_close_sends_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginClose);
    assert_eq!(c.user(Intent::Dismiss), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn rename_and_close_need_a_selected_session() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.user(Intent::BeginRename);
    c.user(Intent::BeginClose);
    assert_eq!(c.client().popup(), None);
}

// ---------- the context menu and the close button ----------

fn cell(col: u16, row: u16) -> Cell {
    Cell { col, row }
}

#[test]
fn opening_a_menu_on_a_session_shows_it_where_it_was_asked_for() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    let sent = c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(4, 3),
    });
    assert_eq!(sent, []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Menu {
            session: sid("s1"),
            at: cell(4, 3)
        })
    );
    assert_eq!(selected(&c).as_deref(), Some("s2"), "the selection stays");
}

#[test]
fn a_menu_on_a_session_that_is_not_listed_does_not_open() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::OpenMenu {
        session: sid("gone"),
        at: cell(0, 0),
    });
    assert_eq!(c.client().popup(), None);
}

#[test]
fn choosing_rename_renames_the_menus_session_not_the_selected_one() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    });
    assert_eq!(c.user(Intent::MenuChoose(MenuItem::Rename)), []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::Rename {
            session: sid("s1"),
            name: "shell-1".into()
        })
    );
    c.user(Intent::EditRename("api".into()));
    let sent = c.user(Intent::Confirm);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionRename(SessionRename {
            session: sid("s1"),
            name: "api".into()
        })]
    );
}

#[test]
fn choosing_close_asks_to_close_the_menus_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    });
    assert_eq!(c.user(Intent::MenuChoose(MenuItem::Close)), []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose { session: sid("s1") })
    );
}

#[test]
fn choosing_with_no_menu_open_does_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::MenuChoose(MenuItem::Close));
    assert_eq!(c.client().popup(), None);
    c.user(Intent::ShowHelp);
    c.user(Intent::MenuChoose(MenuItem::Rename));
    assert_eq!(c.client().popup(), Some(&Popup::Help));
}

#[test]
fn choosing_on_a_session_that_closed_meanwhile_just_closes_the_menu() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    });
    c.host().close("s1");
    c.sync();
    c.user(Intent::MenuChoose(MenuItem::Rename));
    assert_eq!(c.client().popup(), None);
}

#[test]
fn dismissing_a_menu_closes_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    });
    assert_eq!(c.user(Intent::Dismiss), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn confirming_a_menu_does_nothing_but_close_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    });
    assert_eq!(c.user(Intent::Confirm), []);
    assert_eq!(c.client().popup(), None);
}

// ---------- resume ----------

/// A connected client sent shell session `s1`, then Claude Code session `s2` whose agent reported `agent_session`
/// and exited.
fn with_exited_claude(agent_session: Option<&str>) -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().new_session_with("s2", claude_session_spec());
    c.host().agent_started("s2", "claude-code", agent_session);
    c.host().pty_exited("s2", Some(0));
    c.sync();
    c
}

fn open_menu(c: &mut ClientDriver, id: &str) {
    c.user(Intent::OpenMenu {
        session: sid(id),
        at: cell(1, 1),
    });
}

fn entry(item: MenuItem, unavailable: Option<ResumeUnavailable>) -> MenuEntry {
    MenuEntry { item, unavailable }
}

#[test]
fn a_running_sessions_menu_offers_rename_and_close() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.client().menu(), [], "no menu open");
    open_menu(&mut c, "s1");
    assert_eq!(
        c.client().menu(),
        [entry(MenuItem::Rename, None), entry(MenuItem::Close, None)]
    );
}

#[test]
fn an_exited_agent_sessions_menu_offers_resume() {
    let mut c = with_exited_claude(Some("abc123"));
    assert_eq!(c.client().sessions()[0].resume, Ok(()));
    open_menu(&mut c, "s2");
    assert_eq!(
        c.client().menu(),
        [
            entry(MenuItem::Rename, None),
            entry(MenuItem::Close, None),
            entry(MenuItem::Resume, None),
        ]
    );
}

#[test]
fn an_exited_session_with_no_agent_session_id_shows_resume_unavailable() {
    let mut c = with_exited_claude(None);
    open_menu(&mut c, "s2");
    assert_eq!(
        c.client().menu().last(),
        Some(&entry(
            MenuItem::Resume,
            Some(ResumeUnavailable::NoAgentSession)
        ))
    );
}

#[test]
fn choosing_resume_asks_the_host_to_resume_the_menus_session() {
    let mut c = with_exited_claude(Some("abc123"));
    open_menu(&mut c, "s2");
    let sent = c.user(Intent::MenuChoose(MenuItem::Resume));
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionResume(SessionResume {
            session: sid("s2")
        })]
    );
    assert_eq!(c.client().popup(), None);
}

#[test]
fn choosing_an_unavailable_resume_says_why_and_sends_nothing() {
    let mut c = with_exited_claude(None);
    open_menu(&mut c, "s2");
    assert_eq!(c.user(Intent::MenuChoose(MenuItem::Resume)), []);
    assert_eq!(
        c.client().notice(),
        Some("can't resume: no agent session id")
    );
    assert_eq!(c.client().popup(), None);
}

#[test]
fn choosing_resume_on_a_running_session_sends_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    open_menu(&mut c, "s1");
    assert_eq!(c.user(Intent::MenuChoose(MenuItem::Resume)), []);
}

#[test]
fn a_resumed_session_is_selected_and_sized_to_the_view_after_the_resume() {
    let mut c = with_exited_claude(Some("abc123"));
    c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    c.user(Intent::SelectSession(sid("s1")));
    open_menu(&mut c, "s2");
    let sent = c.user(Intent::MenuChoose(MenuItem::Resume));
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    // The host handles a connection's messages in order: the resize reaches the new PTY.
    assert!(
        matches!(
            &sent[..],
            [ClientCommand::Send(ClientMessage::Request(_)), ClientCommand::Send(ClientMessage::Notification(n))]
                if *n == resize("s2", 100, 30)
        ),
        "{sent:?}"
    );
}

#[test]
fn resuming_the_selected_session_resizes_it_again() {
    // Its new PTY starts at the last size the host knows for it, which may not be the view's.
    let mut c = with_exited_claude(Some("abc123"));
    c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    c.user(Intent::SelectSession(sid("s2")));
    open_menu(&mut c, "s2");
    let sent = c.user(Intent::MenuChoose(MenuItem::Resume));
    assert_eq!(notifications(&sent), [&resize("s2", 100, 30)]);
}

#[test]
fn closing_a_given_session_asks_to_close_it_without_selecting_it() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(c.user(Intent::BeginCloseSession(sid("s1"))), []);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::ConfirmClose { session: sid("s1") })
    );
    assert_eq!(selected(&c).as_deref(), Some("s2"));
    let sent = c.user(Intent::Confirm);
    assert_eq!(
        bodies(&sent),
        [ClientRequest::SessionClose(SessionClose {
            session: sid("s1"),
            worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        })]
    );
}

#[test]
fn closing_a_session_that_is_not_listed_does_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginCloseSession(sid("gone")));
    assert_eq!(c.client().popup(), None);
}

/// Every popup about session `s1`, opened from the keyboard (with `s1` selected) or the mouse, as the intents that
/// open it.
fn session_popups() -> Vec<(&'static str, Vec<Intent>)> {
    let menu = Intent::OpenMenu {
        session: sid("s1"),
        at: cell(1, 1),
    };
    vec![
        ("close from the keyboard", vec![Intent::BeginClose]),
        (
            "close from the row's button",
            vec![Intent::BeginCloseSession(sid("s1"))],
        ),
        ("context menu", vec![menu.clone()]),
        (
            "close from the context menu",
            vec![menu.clone(), Intent::MenuChoose(MenuItem::Close)],
        ),
        ("rename from the keyboard", vec![Intent::BeginRename]),
        (
            "rename from the context menu",
            vec![menu, Intent::MenuChoose(MenuItem::Rename)],
        ),
    ]
}

/// `s1` and `s2`, `s1` selected, with the popup that `opens` opens.
fn with_popup(case: &str, opens: Vec<Intent>) -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::SelectSession(sid("s1")));
    for intent in opens {
        c.user(intent);
    }
    assert!(c.client().popup().is_some(), "{case}: the popup opens");
    c
}

#[test]
fn a_popup_about_a_session_closes_when_the_host_removes_that_session() {
    for (case, opens) in session_popups() {
        let mut c = with_popup(case, opens);
        c.host().close("s1");
        c.sync();
        assert_eq!(c.client().popup(), None, "{case}");
    }
}

#[test]
fn a_popup_about_a_session_stays_when_the_host_removes_another_session() {
    for (case, opens) in session_popups() {
        let mut c = with_popup(case, opens);
        let open = c.client().popup().cloned();
        c.host().close("s2");
        c.sync();
        assert_eq!(c.client().popup().cloned(), open, "{case}");
    }
}

#[test]
fn help_stays_open_when_a_session_is_removed() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::ShowHelp);
    c.host().close("s1");
    c.sync();
    assert_eq!(c.client().popup(), Some(&Popup::Help));
}

#[test]
fn quit_quits() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.user(Intent::Quit), [ClientCommand::Quit]);
}

// ---------- responses ----------

#[test]
fn an_error_answer_is_shown_as_a_notice() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginRename);
    let sent = c.user(Intent::Confirm);
    c.answer(&sent, Err(WireError::UnknownSession { session: sid("s1") }));
    assert!(
        c.client().notice().is_some_and(|n| n.contains("s1")),
        "{:?}",
        c.client().notice()
    );
}

/// A connected client with session `s1`, showing a notice (a refused rename).
fn with_notice() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginRename);
    let sent = c.user(Intent::Confirm);
    c.answer(&sent, Err(WireError::Internal("refused".into())));
    assert!(c.client().notice().is_some());
    c
}

#[test]
fn a_notice_is_cleared_by_the_next_user_action() {
    let actions = [
        Intent::NextSession,
        Intent::PrevSession,
        Intent::keys(b"x".to_vec()),
        Intent::SetFocus(Focus::Terminal),
        Intent::SelectSession(sid("s1")),
        Intent::ScrollSidebar(1),
        Intent::ScrollPicker { by: 1, shown: 2 },
        Intent::ScrollSwitcher { by: 1, shown: 2 },
        Intent::NewSession,
        Intent::BeginNewSession,
        Intent::ShowHelp,
        Intent::BeginRename,
        Intent::BeginClose,
        Intent::BeginCloseSession(sid("s1")),
        Intent::OpenMenu {
            session: sid("s1"),
            at: Cell { col: 1, row: 2 },
        },
        Intent::BeginWorkspace,
        Intent::PickProfile("shell".into()),
        Intent::EditRename("x".into()),
        Intent::MenuChoose(MenuItem::Rename),
        Intent::EditWorkspacePath("/tmp/x".into()),
        Intent::PickWorkspace(PathBuf::from("/tmp/x")),
        Intent::Confirm,
        Intent::Dismiss,
        Intent::Quit,
    ];
    for action in actions {
        let mut c = with_notice();
        c.user(action.clone());
        assert_eq!(c.client().notice(), None, "after {action:?}");
    }
}

#[test]
fn layout_reports_are_not_user_actions_and_keep_the_notice() {
    let reports = [
        Intent::Resize(PtySize {
            cols: 100,
            rows: 30,
        }),
        Intent::SidebarRows(5),
    ];
    for report in reports {
        let mut c = with_notice();
        c.user(report.clone());
        assert!(c.client().notice().is_some(), "after {report:?}");
    }
}

#[test]
fn the_window_gaining_or_losing_focus_is_not_a_user_action_and_keeps_the_notice() {
    for focused in [false, true] {
        let mut c = with_notice();
        c.user(Intent::WindowFocus(focused));
        assert!(c.client().notice().is_some(), "after focused = {focused}");
    }
}

#[test]
fn a_notice_raised_by_a_user_action_shows_after_it() {
    let mut c = ClientDriver::new();
    c.user(Intent::NewSession);
    assert_eq!(
        c.client().notice(),
        Some("not connected to the shepherd host")
    );
}

#[test]
fn a_notice_clears_once_it_has_shown_for_its_timeout() {
    let mut c = with_notice();
    let timeout = NOTICE_TIMEOUT.as_millis() as i64;
    // The first tick after it appears starts its time.
    c.handle(ClientEvent::Tick { at: at(1_000) });
    c.handle(ClientEvent::Tick {
        at: at(1_000 + timeout - 1),
    });
    assert!(c.client().notice().is_some());
    c.handle(ClientEvent::Tick {
        at: at(1_000 + timeout),
    });
    assert_eq!(c.client().notice(), None);
}

#[test]
fn a_new_notice_gets_its_own_timeout() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let timeout = NOTICE_TIMEOUT.as_millis() as i64;
    // Two requests in flight, refused one after the other with no user action in between.
    c.user(Intent::BeginRename);
    let first = c.user(Intent::Confirm);
    c.user(Intent::BeginRename);
    let second = c.user(Intent::Confirm);
    c.answer(&first, Err(WireError::Internal("first".into())));
    c.handle(ClientEvent::Tick { at: at(0) });
    c.handle(ClientEvent::Tick {
        at: at(timeout - 1),
    });
    c.answer(&second, Err(WireError::Internal("second".into())));
    c.handle(ClientEvent::Tick { at: at(timeout) });
    assert!(
        c.client().notice().is_some_and(|n| n.contains("second")),
        "{:?}",
        c.client().notice()
    );
}

#[test]
fn an_answer_to_no_pending_request_is_ignored() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let sent = c.user(Intent::NewSession);
    c.answer(&sent, Ok(json!({ "session": "s9" })));
    // The same id again: no longer pending.
    let again = c.answer(&sent, Err(WireError::Internal("late".into())));
    assert_eq!(again, []);
    assert_eq!(c.client().notice(), None);
}

// ---------- connecting ----------

#[test]
fn starting_connects() {
    let mut c = ClientDriver::new();
    assert_eq!(c.handle(ClientEvent::Start), [ClientCommand::Connect]);
    assert_eq!(
        c.client().connection(),
        &Connection::Connecting { attempt: 1 }
    );
}

#[test]
fn connecting_says_hello_with_this_version_and_machine() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let sent = c.handle(connected());
    assert_eq!(
        bodies(&sent),
        [ClientRequest::Hello(shepherd_core::wire::Hello {
            version: PROTOCOL_VERSION,
            machine: shepherd_core::ids::MachineId::new("m_client"),
            cwd: None,
            build: None,
        })]
    );
}

#[test]
fn a_hello_answer_subscribes_to_everything_and_is_ready() {
    let mut c = ClientDriver::new();
    let sent = c.connect();
    assert_eq!(
        bodies(&sent),
        [ClientRequest::Subscribe(Default::default())]
    );
    assert_eq!(c.client().connection(), &Connection::Ready);
}

#[test]
fn a_hello_refused_for_another_reason_than_the_version_is_fatal() {
    // A version mismatch is no longer fatal (T36a): see `client_build.rs`.
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(connected());
    let cmds = c.answer(&hello, Err(WireError::Internal("no machine id".into())));
    assert_eq!(cmds, [ClientCommand::Quit]);
    assert!(
        c.client()
            .fatal()
            .is_some_and(|m| m.contains("no machine id"))
    );
}

#[test]
fn a_failed_connect_launches_the_host_and_waits() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let cmds = c.handle(ClientEvent::ConnectFailed {
        at: ts("2026-10-29T12:00:00.000Z"),
        error: "connection refused".into(),
    });
    assert_eq!(cmds, [ClientCommand::LaunchHost]);
    assert!(matches!(
        c.client().connection(),
        Connection::Waiting { attempt: 1, .. }
    ));
}

#[test]
fn a_tick_before_the_retry_interval_does_nothing() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    c.handle(ClientEvent::ConnectFailed {
        at: ts("2026-10-29T12:00:00.000Z"),
        error: "refused".into(),
    });
    let early = ts("2026-10-29T12:00:00.050Z");
    assert_eq!(c.handle(ClientEvent::Tick { at: early }), []);
}

#[test]
fn a_tick_after_the_retry_interval_connects_again() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    c.handle(ClientEvent::ConnectFailed {
        at: ts("2026-10-29T12:00:00.000Z"),
        error: "refused".into(),
    });
    let later = ts("2026-10-29T12:00:00.100Z");
    assert_eq!(
        c.handle(ClientEvent::Tick { at: later }),
        [ClientCommand::Connect]
    );
    assert_eq!(
        c.client().connection(),
        &Connection::Connecting { attempt: 2 }
    );
}

/// `2026-10-29T12:00:00Z` plus `ms` milliseconds.
fn at(ms: i64) -> Timestamp {
    let t = ts("2026-10-29T12:00:00.000Z")
        .instant()
        .checked_add(jiff::SignedDuration::from_millis(ms))
        .unwrap_or_else(|e| panic!("{e}"));
    ts(&t.to_string())
}

/// A connect attempt that fails at `ms`, and the commands it produced.
fn refused(c: &mut ClientDriver, ms: i64) -> Vec<ClientCommand> {
    c.handle(ClientEvent::ConnectFailed {
        at: at(ms),
        error: "connection refused".into(),
    })
}

#[test]
fn the_host_is_launched_once_per_outage() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let mut launches = 0;
    for i in 0..5 {
        let cmds = refused(&mut c, i * 1000);
        launches += cmds
            .iter()
            .filter(|c| **c == ClientCommand::LaunchHost)
            .count();
        c.handle(ClientEvent::Tick {
            at: at(i * 1000 + 500),
        });
    }
    assert_eq!(launches, 1);
}

#[test]
fn giving_up_after_the_last_attempt_is_a_clear_fatal_error() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let mut last = Vec::new();
    for i in 0..i64::from(MAX_CONNECT_ATTEMPTS) {
        last = refused(&mut c, i * 1000);
        if i + 1 < i64::from(MAX_CONNECT_ATTEMPTS) {
            assert_eq!(
                c.handle(ClientEvent::Tick {
                    at: at(i * 1000 + 500)
                }),
                [ClientCommand::Connect],
                "attempt {i}"
            );
        }
    }
    assert_eq!(last, [ClientCommand::Quit]);
    let fatal = c.client().fatal().unwrap_or_default().to_owned();
    assert!(fatal.contains("shepherd host"), "{fatal}");
    assert!(fatal.contains("connection refused"), "{fatal}");
    assert!(fatal.contains(&MAX_CONNECT_ATTEMPTS.to_string()), "{fatal}");
}

#[test]
fn a_failed_launch_is_a_clear_fatal_error() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    c.handle(ClientEvent::ConnectFailed {
        at: ts("2026-10-29T12:00:00.000Z"),
        error: "refused".into(),
    });
    let cmds = c.handle(ClientEvent::LaunchFailed {
        error: "no such file".into(),
    });
    assert_eq!(cmds, [ClientCommand::Quit]);
    let fatal = c.client().fatal().unwrap_or_default().to_owned();
    assert!(fatal.contains("start the shepherd host"), "{fatal}");
    assert!(fatal.contains("no such file"), "{fatal}");
}

/// Whatever the launcher reports, from a missing executable to the host's own startup error, reaches the user
/// whole, and the client gives up at once: retrying can't fix it.
#[test]
fn a_failed_launch_shows_its_message_quits_and_ignores_what_follows() {
    let cases = [
        "could not start the host process: /usr/bin/shepherd: no such file",
        "shepherd host: /home/me/.shepherd has mode 755, but it holds your agents' logs and the host socket, \
         so only you may access it. Fix it with: chmod 700 /home/me/.shepherd",
        "shepherd host: first line\nsecond line",
    ];
    for error in cases {
        let mut c = ClientDriver::new();
        c.handle(ClientEvent::Start);
        let launch = c.handle(ClientEvent::ConnectFailed {
            at: at(0),
            error: "connection refused".into(),
        });
        assert_eq!(launch, [ClientCommand::LaunchHost], "{error}");
        let cmds = c.handle(ClientEvent::LaunchFailed {
            error: error.into(),
        });
        assert_eq!(cmds, [ClientCommand::Quit], "{error}");
        let fatal = c.client().fatal().unwrap_or_default().to_owned();
        assert!(fatal.contains(error), "{fatal}");
        assert!(matches!(c.client().connection(), Connection::Failed(_)));
        assert!(
            c.handle(ClientEvent::Tick { at: at(10_000) }).is_empty(),
            "{error}: no retry after giving up"
        );
    }
}

#[test]
fn losing_the_host_reconnects_and_resumes_each_log_where_it_left_off() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().rename("s1", "api");
    c.sync();
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    let hello = c.handle(connected());
    let sent = c.answer(
        &hello,
        Ok(json!({ "version": PROTOCOL_VERSION, "machine": "m_test" })),
    );
    let [ClientRequest::Subscribe(sub)] = &bodies(&sent)[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(sub.from.host, None);
    assert_eq!(sub.from.sessions.get(&sid("s1")).map(|s| s.get()), Some(2));
    // The view is kept across the outage.
    assert_eq!(ids(&c), ["s1"]);
}

#[test]
fn a_reconnect_that_is_refused_launches_the_host_again() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    assert_eq!(refused(&mut c, 0), [ClientCommand::LaunchHost]);
    c.handle(ClientEvent::Tick { at: at(100) });
    let hello = c.handle(connected_at(100));
    c.answer(&hello, Ok(hello_ok()));
    assert_eq!(c.client().connection(), &Connection::Ready);
    // The launched host dies later: a new outage.
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::Connect]
    );
    assert_eq!(refused(&mut c, 5000), [ClientCommand::LaunchHost]);
}

#[test]
fn a_host_that_drops_before_answering_hello_is_a_failed_attempt() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    c.handle(connected_at(0));
    assert_eq!(
        c.handle(ClientEvent::Disconnected),
        [ClientCommand::LaunchHost]
    );
    assert!(matches!(
        c.client().connection(),
        Connection::Waiting { attempt: 1, .. }
    ));
    assert_eq!(
        c.handle(ClientEvent::Tick { at: at(100) }),
        [ClientCommand::Connect]
    );
    assert_eq!(
        c.client().connection(),
        &Connection::Connecting { attempt: 2 }
    );
}

#[test]
fn a_host_that_keeps_dropping_the_handshake_is_launched_once_then_given_up_on() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let (mut launches, mut last) = (0, Vec::new());
    for i in 0..i64::from(MAX_CONNECT_ATTEMPTS) {
        c.handle(connected_at(i * 1000));
        last = c.handle(ClientEvent::Disconnected);
        launches += last
            .iter()
            .filter(|c| **c == ClientCommand::LaunchHost)
            .count();
        c.handle(ClientEvent::Tick {
            at: at(i * 1000 + 500),
        });
    }
    assert_eq!(launches, 1);
    assert_eq!(last, [ClientCommand::Quit]);
    let fatal = c.client().fatal().unwrap_or_default().to_owned();
    assert!(fatal.contains("closed the connection"), "{fatal}");
}

#[test]
fn a_host_that_never_answers_hello_times_out_as_a_failed_attempt() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(connected_at(0));
    let timeout_ms = HELLO_TIMEOUT.as_millis() as i64;
    assert_eq!(
        c.handle(ClientEvent::Tick {
            at: at(timeout_ms - 1)
        }),
        []
    );
    assert_eq!(
        c.handle(ClientEvent::Tick { at: at(timeout_ms) }),
        [ClientCommand::LaunchHost]
    );
    assert!(matches!(
        c.client().connection(),
        Connection::Waiting { attempt: 1, .. }
    ));
    // A hello answer that arrives too late is ignored.
    assert_eq!(c.answer(&hello, Ok(hello_ok())), []);
    assert_ne!(c.client().connection(), &Connection::Ready);
}

#[test]
fn answers_pending_when_the_link_dropped_are_forgotten() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let sent = c.user(Intent::NewSession);
    c.handle(ClientEvent::Disconnected);
    c.handle(connected());
    assert_eq!(c.answer(&sent, Err(WireError::Internal("x".into()))), []);
    assert_eq!(c.client().notice(), None);
}

// ---------- profiles ----------

/// A connected client with session `s1`, whose host has the `claude`, `codex` and `shell` profiles, `codex` the
/// default.
fn with_profiles() -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("claude", AgentKind::ClaudeCode),
            ("codex", AgentKind::Codex),
            ("shell", AgentKind::Shell),
        ],
        "codex",
    );
    c.host().new_session("s1");
    c.sync();
    c
}

fn new_session(profile: Option<&str>) -> ClientRequest {
    ClientRequest::SessionNew(SessionNew {
        profile: profile.map(str::to_owned),
        ..SessionNew::default()
    })
}

#[test]
fn the_client_keeps_the_hosts_profiles_default_first() {
    let c = with_profiles();
    let names: Vec<&str> = c
        .client()
        .profiles()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["codex", "claude", "shell"]);
    assert_eq!(c.client().default_profile(), Some("codex"));
}

#[test]
fn new_session_with_several_profiles_opens_the_picker() {
    let mut c = with_profiles();
    assert_eq!(c.user(Intent::BeginNewSession), []);
    assert_eq!(c.client().popup(), Some(&Popup::PickProfile));
}

#[test]
fn picking_a_profile_creates_a_session_with_it() {
    let mut c = with_profiles();
    c.user(Intent::BeginNewSession);
    let sent = c.user(Intent::PickProfile("shell".into()));
    assert_eq!(bodies(&sent), [new_session(Some("shell"))]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn picking_a_profile_without_the_picker_open_does_nothing() {
    let mut c = with_profiles();
    assert_eq!(c.user(Intent::PickProfile("shell".into())), []);
    c.user(Intent::ShowHelp);
    assert_eq!(c.user(Intent::PickProfile("shell".into())), []);
    assert_eq!(c.client().popup(), Some(&Popup::Help));
}

#[test]
fn dismissing_the_picker_creates_nothing() {
    let mut c = with_profiles();
    c.user(Intent::BeginNewSession);
    assert_eq!(c.user(Intent::Dismiss), []);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn confirm_on_the_picker_creates_nothing() {
    let mut c = with_profiles();
    c.user(Intent::BeginNewSession);
    assert_eq!(c.user(Intent::Confirm), []);
}

#[test]
fn new_session_with_one_profile_creates_it_directly() {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(&[("claude", AgentKind::ClaudeCode)], "claude");
    let sent = c.user(Intent::BeginNewSession);
    assert_eq!(bodies(&sent), [new_session(Some("claude"))]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn new_session_from_a_host_without_profiles_creates_the_default_directly() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let sent = c.user(Intent::BeginNewSession);
    assert_eq!(bodies(&sent), [new_session(None)]);
    assert_eq!(c.client().popup(), None);
}

#[test]
fn the_new_session_key_creates_the_default_profile_without_the_picker() {
    let mut c = with_profiles();
    let sent = c.user(Intent::NewSession);
    assert_eq!(bodies(&sent), [new_session(None)]);
    assert_eq!(c.client().popup(), None);
}

/// A connected client whose host has `n` profiles, `p01` (the default) to `p<n>`, with the picker open.
fn picker_of(n: usize) -> ClientDriver {
    let names: Vec<String> = (1..=n).map(|i| format!("p{i:02}")).collect();
    let profiles: Vec<(&str, AgentKind)> = names
        .iter()
        .map(|n| (n.as_str(), AgentKind::Shell))
        .collect();
    let mut c = ClientDriver::new();
    c.connect_with_profiles(&profiles, "p01");
    c.user(Intent::BeginNewSession);
    c
}

#[test]
fn the_picker_scrolls_within_its_list() {
    let mut c = picker_of(10);
    assert_eq!(c.client().picker_scroll(), 0);
    assert_eq!(c.user(Intent::ScrollPicker { by: 3, shown: 4 }), []);
    assert_eq!(c.client().picker_scroll(), 3);
    // The last rows fill the picker: 10 profiles, 4 shown.
    c.user(Intent::ScrollPicker { by: 100, shown: 4 });
    assert_eq!(c.client().picker_scroll(), 6);
    c.user(Intent::ScrollPicker { by: -2, shown: 4 });
    assert_eq!(c.client().picker_scroll(), 4);
    c.user(Intent::ScrollPicker { by: -100, shown: 4 });
    assert_eq!(c.client().picker_scroll(), 0);
}

#[test]
fn scrolling_up_a_picker_that_grew_starts_from_its_new_end() {
    let mut c = picker_of(10);
    c.user(Intent::ScrollPicker { by: 100, shown: 4 });
    assert_eq!(c.client().picker_scroll(), 6);
    // Taller now, showing 8, so its end is 2 rows down; up one from there.
    c.user(Intent::ScrollPicker { by: -1, shown: 8 });
    assert_eq!(c.client().picker_scroll(), 1);
}

#[test]
fn a_picker_that_shows_every_profile_does_not_scroll() {
    let mut c = picker_of(3);
    c.user(Intent::ScrollPicker { by: 1, shown: 3 });
    assert_eq!(c.client().picker_scroll(), 0);
}

#[test]
fn the_picker_opens_scrolled_to_the_top() {
    let mut c = picker_of(10);
    c.user(Intent::ScrollPicker { by: 5, shown: 4 });
    c.user(Intent::Dismiss);
    c.user(Intent::BeginNewSession);
    assert_eq!(c.client().picker_scroll(), 0);
}

#[test]
fn scrolling_the_picker_while_it_is_closed_does_nothing() {
    let mut c = picker_of(10);
    c.user(Intent::Dismiss);
    assert_eq!(c.user(Intent::ScrollPicker { by: 5, shown: 4 }), []);
    assert_eq!(c.client().picker_scroll(), 0);
    c.user(Intent::BeginNewSession);
    assert_eq!(c.client().picker_scroll(), 0);
}

// ---------- workspaces ----------

fn workspace_popup(path: &str, error: Option<&str>) -> Popup {
    Popup::Workspace {
        path: path.to_owned(),
        error: error.map(str::to_owned),
    }
}

fn select_workspace(path: &str) -> ClientRequest {
    ClientRequest::WorkspaceSelect(WorkspaceSelect {
        path: path.to_owned(),
    })
}

#[test]
fn the_hello_carries_the_directory_the_client_started_in() {
    let mut c = ClientDriver::from_client(
        Client::new(MachineId::new("m_client")).with_cwd("/src/app/crates"),
    );
    c.handle(ClientEvent::Start);
    let sent = c.handle(connected());
    match &bodies(&sent)[..] {
        [ClientRequest::Hello(hello)] => assert_eq!(hello.cwd.as_deref(), Some("/src/app/crates")),
        other => panic!("expected a hello, got {other:?}"),
    }
}

#[test]
fn the_client_knows_the_hosts_workspace_and_the_ones_used_before() {
    let mut c = ClientDriver::with_sessions(&[]);
    assert_eq!(c.client().workspace(), None);
    c.host().select_workspace("/src/a");
    c.host().select_workspace("/src/b");
    c.sync();
    assert_eq!(c.client().workspace(), Some(Path::new("/src/b")));
    assert_eq!(
        c.client().workspaces(),
        [PathBuf::from("/src/b"), PathBuf::from("/src/a")]
    );
}

#[test]
fn sessions_from_another_workspace_are_listed_and_marked() {
    let mut c = ClientDriver::with_sessions(&["s0"]);
    c.host().select_workspace("/src/a");
    c.host().new_session("s1");
    c.host().select_workspace("/src/b");
    c.host().new_session("s2");
    c.sync();
    let marks: Vec<(String, bool)> = c
        .client()
        .sessions()
        .iter()
        .map(|s| (s.id.to_string(), s.elsewhere))
        .collect();
    // s0 predates any workspace, so it isn't from another one.
    assert_eq!(
        marks,
        [
            ("s2".to_owned(), false),
            ("s1".to_owned(), true),
            ("s0".to_owned(), false)
        ]
    );
    assert_eq!(
        c.client().sessions()[1].workspace.as_deref(),
        Some(Path::new("/src/a"))
    );
}

#[test]
fn switching_back_unmarks_that_workspaces_sessions() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/a");
    c.host().new_session("s1");
    c.host().select_workspace("/src/b");
    c.sync();
    assert!(c.client().sessions()[0].elsewhere);
    c.host().select_workspace("/src/a");
    c.sync();
    assert!(!c.client().sessions()[0].elsewhere);
}

#[test]
fn clicking_the_workspace_opens_the_switcher_with_an_empty_path() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    assert_eq!(c.user(Intent::BeginWorkspace), []);
    assert_eq!(c.client().popup(), Some(&workspace_popup("", None)));
}

#[test]
fn opening_a_typed_path_asks_the_host_to_select_it_and_waits_for_the_answer() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/src/app".into()));
    let sent = c.user(Intent::Confirm);
    assert_eq!(bodies(&sent), [select_workspace("/src/app")]);
    assert_eq!(c.client().popup(), Some(&workspace_popup("/src/app", None)));
    c.answer(&sent, Ok(json!({})));
    assert_eq!(c.client().popup(), None);
}

#[test]
fn opening_an_empty_path_says_so_and_sends_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("  ".into()));
    assert_eq!(c.user(Intent::Confirm), []);
    assert_eq!(
        c.client().popup(),
        Some(&workspace_popup("  ", Some("type the path of a repo")))
    );
}

#[test]
fn a_path_the_host_refuses_shows_why_in_the_switcher() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = c.user(Intent::Confirm);
    let why = "/tmp/x is not a repo";
    c.answer(&sent, Err(WireError::InvalidParams(why.into())));
    assert_eq!(
        c.client().popup(),
        Some(&workspace_popup("/tmp/x", Some(why)))
    );
    assert_eq!(c.client().notice(), None);
    c.user(Intent::EditWorkspacePath("/tmp/y".into()));
    assert_eq!(c.client().popup(), Some(&workspace_popup("/tmp/y", None)));
}

#[test]
fn a_refusal_after_the_switcher_closed_is_a_notice() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::BeginWorkspace);
    c.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = c.user(Intent::Confirm);
    c.user(Intent::Dismiss);
    c.answer(&sent, Err(WireError::InvalidParams("nope".into())));
    assert_eq!(c.client().popup(), None);
    assert!(c.client().notice().is_some_and(|n| n.contains("nope")));
}

#[test]
fn picking_a_workspace_used_before_asks_the_host_to_select_it() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().select_workspace("/src/a");
    c.host().select_workspace("/src/b");
    c.sync();
    assert_eq!(c.user(Intent::PickWorkspace("/src/a".into())), []);
    c.user(Intent::BeginWorkspace);
    let sent = c.user(Intent::PickWorkspace("/src/a".into()));
    assert_eq!(bodies(&sent), [select_workspace("/src/a")]);
    c.answer(&sent, Ok(json!({})));
    assert_eq!(c.client().popup(), None);
}

/// A connected client whose host has used `n` workspaces, `/src/w01` (the oldest) to `/src/w<n>` (current), with the
/// switcher open.
fn switcher_of(n: usize) -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    for i in 1..=n {
        c.host().select_workspace(&format!("/src/w{i:02}"));
    }
    c.sync();
    c.user(Intent::BeginWorkspace);
    c
}

#[test]
fn the_switcher_scrolls_within_its_list() {
    let mut c = switcher_of(10);
    assert_eq!(c.client().switcher_scroll(), 0);
    assert_eq!(c.user(Intent::ScrollSwitcher { by: 3, shown: 4 }), []);
    assert_eq!(c.client().switcher_scroll(), 3);
    // The last rows fill the switcher: 10 workspaces, 4 shown.
    c.user(Intent::ScrollSwitcher { by: 100, shown: 4 });
    assert_eq!(c.client().switcher_scroll(), 6);
    c.user(Intent::ScrollSwitcher { by: -2, shown: 4 });
    assert_eq!(c.client().switcher_scroll(), 4);
    c.user(Intent::ScrollSwitcher { by: -100, shown: 4 });
    assert_eq!(c.client().switcher_scroll(), 0);
}

#[test]
fn scrolling_up_a_switcher_that_grew_starts_from_its_new_end() {
    let mut c = switcher_of(10);
    c.user(Intent::ScrollSwitcher { by: 100, shown: 4 });
    // Taller now, showing 8, so its end is 2 rows down; up one from there.
    c.user(Intent::ScrollSwitcher { by: -1, shown: 8 });
    assert_eq!(c.client().switcher_scroll(), 1);
}

#[test]
fn a_switcher_that_shows_every_workspace_does_not_scroll() {
    let mut c = switcher_of(3);
    c.user(Intent::ScrollSwitcher { by: 1, shown: 3 });
    assert_eq!(c.client().switcher_scroll(), 0);
}

#[test]
fn the_switcher_opens_scrolled_to_the_top() {
    let mut c = switcher_of(10);
    c.user(Intent::ScrollSwitcher { by: 5, shown: 4 });
    c.user(Intent::Dismiss);
    c.user(Intent::BeginWorkspace);
    assert_eq!(c.client().switcher_scroll(), 0);
}

#[test]
fn scrolling_the_switcher_while_it_is_closed_does_nothing() {
    let mut c = switcher_of(10);
    c.user(Intent::Dismiss);
    assert_eq!(c.user(Intent::ScrollSwitcher { by: 5, shown: 4 }), []);
    assert_eq!(c.client().switcher_scroll(), 0);
}

#[test]
fn a_refused_path_keeps_the_switcher_scrolled() {
    let mut c = switcher_of(10);
    c.user(Intent::ScrollSwitcher { by: 5, shown: 4 });
    c.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = c.user(Intent::Confirm);
    c.answer(&sent, Err(WireError::InvalidParams("nope".into())));
    assert_eq!(c.client().switcher_scroll(), 5);
}

// ---------- worktrees ----------

#[test]
fn a_session_shows_while_its_worktree_is_made_and_why_it_failed() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/app");
    c.host().new_session("s1");
    c.host().new_session("s2");
    c.sync();
    let state = |c: &ClientDriver| -> Vec<(bool, Option<String>)> {
        c.client()
            .sessions()
            .iter()
            .map(|s| (s.creating_worktree, s.failed.clone()))
            .collect()
    };
    assert_eq!(state(&c), [(true, None), (true, None)]);
    c.host().worktree_ready("s1");
    let offline = WorktreeError::FetchFailed("offline".to_owned());
    c.host().worktree_result("s2", Err(offline));
    c.sync();
    // Newest first: s2 failed, s1 runs in its worktree.
    assert_eq!(
        state(&c),
        [
            (false, Some("fetching origin failed: offline".to_owned())),
            (false, None)
        ]
    );
}

#[test]
fn a_session_whose_post_create_hook_failed_shows_a_warning_once_it_runs() {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace("/src/app");
    c.host().new_session("s1");
    c.host().worktree_ready_with_hooks("s1", &["npm ci"]);
    c.sync();
    let state = |c: &ClientDriver| {
        let s = &c.client().sessions()[0];
        (s.creating_worktree, s.warning.clone())
    };
    assert_eq!(state(&c), (true, None));
    c.host().hook_exited("s1", "npm ci", 1);
    c.sync();
    assert_eq!(
        state(&c),
        (false, Some("hook failed (exit 1): npm ci".to_owned()))
    );
}
