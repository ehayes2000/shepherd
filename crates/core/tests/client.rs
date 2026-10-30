//! The client's view state and decisions, through the public `shepherd_core::client` API.

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind, Timestamp};
use shepherd_core::client::{
    Cell, ClientCommand, ClientEvent, Connection, Focus, HELLO_TIMEOUT, Intent,
    MAX_CONNECT_ATTEMPTS, MenuItem, Popup,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::session::Activity;
use shepherd_core::wire::{
    ClientNotification, ClientRequest, PROTOCOL_VERSION, SessionClose, SessionInput, SessionRename,
    SessionResize, Subscribe, WireError,
};
use shepherd_testkit::builders::{ClientDriver, log_entry, notifications, requests, ts};

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

// ---------- input and resize ----------

#[test]
fn keystrokes_in_terminal_focus_go_to_the_selected_session() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    assert_eq!(c.client().focus(), Focus::Terminal);
    let sent = c.user(Intent::Input(b"ls\r".to_vec()));
    assert_eq!(notifications(&sent), [&input("s2", b"ls\r")]);
}

#[test]
fn keystrokes_in_command_focus_are_not_sent() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::SetFocus(Focus::Command));
    assert_eq!(c.client().focus(), Focus::Command);
    assert_eq!(c.user(Intent::Input(b"x".to_vec())), []);
}

#[test]
fn keystrokes_with_no_session_are_dropped() {
    let mut c = ClientDriver::with_sessions(&[]);
    assert_eq!(c.user(Intent::Input(b"x".to_vec())), []);
}

#[test]
fn keystrokes_while_a_popup_is_open_are_not_sent() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::ShowHelp);
    assert_eq!(c.user(Intent::Input(b"x".to_vec())), []);
}

#[test]
fn keystrokes_before_connecting_are_dropped() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    assert_eq!(c.user(Intent::Input(b"x".to_vec())), []);
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
            session: sid("s1")
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
            session: sid("s1")
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
fn a_version_mismatch_is_fatal() {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(connected());
    let cmds = c.answer(
        &hello,
        Err(WireError::VersionMismatch { host: 2, client: 1 }),
    );
    assert_eq!(cmds, [ClientCommand::Quit]);
    assert!(c.client().fatal().is_some_and(|m| m.contains("version")));
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
