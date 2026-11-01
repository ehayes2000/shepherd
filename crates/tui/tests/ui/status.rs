//! The status page's table (T57): every session on every machine, grouped by state or by machine, a header counting
//! each group, clickable rows that scroll with the selection, and attaching a session from it.

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::status::Grouping;
use shepherd_core::client::{ClientCommand, ClientEvent, Focus, PeerEvent};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{Content, LayoutOp, Pane, SplitDir, ViewKind};
use shepherd_core::wire::{
    ClientMessage, HostMessage, HostNotification, LogEntry, PROTOCOL_VERSION, RawResponse,
};
use shepherd_testkit::builders::{ClientDriver, HostDriver, claude_session_spec, ts};
use shepherd_tui::Target;
use shepherd_tui::layout::layout;

use crate::harness::Ui;

pub(crate) fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

pub(crate) fn hook(ui: &mut Ui, session: &str, method: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    ui.on_host(|h| {
        h.append(Some(session), source, frame);
    });
}

pub(crate) fn shepherd_frame(host: &mut HostDriver, session: &str, method: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    host.append(
        Some(session),
        Source::new(SourceKind::Shepherd, None),
        frame,
    );
}

/// A host log with a team of agents (oldest first): `fix-login` (idle, PR #409 passing), `wire-v2` (exited),
/// `f36-cleanup-view` (working on a tool, two subagents, PR #417 pending), `t28-hello-build` (asking permission, PR
/// #416 failing) and `docs-arch` (working, no agent reported). Claude Code sessions, except `docs-arch`.
fn team(host: &mut HostDriver) {
    for (id, name) in [
        ("s1", "fix-login"),
        ("s2", "wire-v2"),
        ("s3", "f36-cleanup-view"),
        ("s4", "t28-hello-build"),
    ] {
        host.new_session_with(id, claude_session_spec());
        host.rename(id, name);
    }
    host.new_session("s5");
    host.rename("s5", "docs-arch");
    let pr = |number: u64, checks: &str| json!({ "pr": { "number": number, "state": "open", "checks": checks } });
    shepherd_frame(host, "s1", "_shepherd/session/pr", pr(409, "passing"));
    shepherd_frame(host, "s3", "_shepherd/session/pr", pr(417, "pending"));
    shepherd_frame(host, "s4", "_shepherd/session/pr", pr(416, "failing"));
    let hook_frame = |host: &mut HostDriver, session: &str, frame: Value| {
        let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
        host.append(Some(session), source, frame);
    };
    hook_frame(
        host,
        "s1",
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} }),
    );
    host.pty_exited("s2", Some(1));
    host.user_prompt("s3", "build the cleanup view");
    hook_frame(
        host,
        "s3",
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "a1", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit cleanup.rs", "kind": "edit",
            "status": "in_progress" } } }),
    );
    for (id, kind) in [("a1", "builder"), ("a2", "reviewer")] {
        hook_frame(
            host,
            "s3",
            json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/started",
                    "params": { "id": id, "agent_type": kind } }),
        );
    }
    host.user_prompt("s4", "add the build id to the hello");
    hook_frame(
        host,
        "s4",
        json!({ "jsonrpc": "2.0", "id": 9, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t9", "title": "Bash(cargo test)" },
                            "options": [] } }),
    );
    host.user_prompt("s5", "write the arch doc");
}

/// The team on a host called `studio`, in a `cols` x `rows` window, the status page open.
pub(crate) fn status_page(cols: u16, rows: u16) -> Ui {
    let mut driver = ClientDriver::new();
    let subscribe = driver.connect_named("studio");
    team(driver.host());
    driver.sync();
    driver.answer(&subscribe, Ok(json!({})));
    let mut ui = Ui::new(driver, cols, rows);
    open_status(&mut ui);
    ui
}

/// `ctrl-b s`, and the host does the layout op it asks for.
pub(crate) fn open_status(ui: &mut Ui) {
    ui.ctrl('b').key(KeyCode::Char('s'));
    ui.apply_layout_ops();
}

/// The pane area.
pub(crate) fn pane(ui: &Ui) -> Rect {
    layout(ui.window()).pane
}

/// The inside of the table's box.
pub(crate) fn table(ui: &Ui) -> Rect {
    ui.boxed("agents")
}

/// The line of the table `name` is drawn on (exactly once there).
fn row_of(ui: &Ui, name: &str) -> Rect {
    let area = table(ui);
    let at = ui.at_in(area, name);
    Rect::new(area.x, at.y, area.width, 1)
}

/// Whether `text` is drawn in the table.
fn in_table(ui: &Ui, text: &str) -> bool {
    !ui.find_in(table(ui), text).is_empty()
}

/// The name of listed session `id`.
fn name_of(ui: &Ui, id: &SessionId) -> String {
    ui.client()
        .sessions()
        .iter()
        .find(|s| &s.id == id)
        .map(|s| s.name.clone())
        .unwrap_or_default()
}

// ---------- the table ----------

#[test]
fn ctrl_b_s_opens_a_status_page_and_focuses_it() {
    let ui = status_page(120, 30);
    let host = ui.client().host_layout();
    assert_eq!(host.pages().len(), 2);
    assert_eq!(host.current_page().name, "status");
    assert!(matches!(
        host.pane(ui.client().focused_pane()),
        Some(Pane::Leaf {
            content: Content::View(ViewKind::Status),
            ..
        })
    ));
    ui.assert_lacks("isn't available yet");
}

#[test]
fn the_table_grouped_by_state() {
    let ui = status_page(120, 30);
    insta::assert_snapshot!(ui.region(pane(&ui)));
}

#[test]
fn the_table_grouped_by_machine() {
    let mut ui = status_page(120, 30);
    ui.click("[g: by machine]");
    assert_eq!(ui.client().status_grouping(), Grouping::Machine);
    ui.assert_contains("[g: by state]");
    insta::assert_snapshot!(ui.region(pane(&ui)));
    ui.click("[g: by state]");
    assert_eq!(ui.client().status_grouping(), Grouping::State);
}

#[test]
fn g_switches_the_grouping() {
    let mut ui = status_page(120, 30);
    ui.key(KeyCode::Char('g'));
    assert_eq!(ui.client().status_grouping(), Grouping::Machine);
    ui.key(KeyCode::Char('g'));
    assert_eq!(ui.client().status_grouping(), Grouping::State);
}

#[test]
fn the_header_counts_each_group() {
    let ui = status_page(120, 30);
    ui.assert_contains("▲ 1 need you")
        .assert_contains("● 2 working")
        .assert_contains("○ 1 idle")
        .assert_contains("✕ 1 exited")
        .assert_contains("↳ 2 subagents")
        .assert_contains("1 machine · 1 online");
    ui.assert_contains("NEEDS YOU  1")
        .assert_contains("WORKING  2")
        .assert_contains("IDLE  1")
        .assert_contains("EXITED  1");
}

#[test]
fn a_row_shows_agent_machine_subagents_doing_and_pr() {
    // Wide enough for every column beside the preview (T58), which narrows the table in a 120-column window.
    let ui = status_page(200, 30);
    let row = row_of(&ui, "f36-cleanup-view");
    let text = ui.region(row);
    for part in ["●", "claude", "studio", "↳2", "Edit cleanup.rs", "#417 …"] {
        assert!(text.contains(part), "{part:?} missing from {text:?}");
    }
    let asking = ui.region(row_of(&ui, "t28-hello-build"));
    assert!(asking.contains("asks: Bash(cargo test)"), "{asking}");
    assert!(asking.contains("#416 ✗"), "{asking}");
    let shell = ui.region(row_of(&ui, "docs-arch"));
    assert!(shell.contains('—'), "no subagents or PR: {shell}");
}

#[test]
fn machine_names_are_drawn_in_their_color() {
    let ui = status_page(120, 30);
    let at = ui.at_in(row_of(&ui, "fix-login"), "studio");
    let color = ui.buffer()[(at.x, at.y)].fg;
    let expected =
        shepherd_tui::machine::color(ui.client().machine_color(&MachineId::new("m_test")));
    assert_eq!(color, expected);
}

// ---------- selecting and attaching ----------

#[test]
fn clicking_a_row_selects_it_in_the_table_only() {
    let mut ui = status_page(120, 30);
    let sidebar_selection = ui.client().selected().cloned();
    let at = ui.at_in(table(&ui), "wire-v2");
    assert!(matches!(
        ui.hit_at(at.x, at.y),
        Some(Target::StatusRow { .. })
    ));
    ui.click_at(at.x, at.y);
    assert_eq!(ui.client().status_selected(), Some(&sid("s2")));
    assert_eq!(ui.client().selected(), sidebar_selection.as_ref());
    assert!(ui.layout_ops().len() == 1, "only the page was asked for");
}

#[test]
fn clicking_the_selected_row_again_attaches_it() {
    let mut ui = status_page(120, 30);
    let at = ui.at_in(table(&ui), "wire-v2");
    ui.click_at(at.x, at.y).click_at(at.x, at.y);
    assert_eq!(ui.client().selected(), Some(&sid("s2")));
    ui.apply_layout_ops();
    // The first page, which shows the selection, is current again and focused.
    let host = ui.client().host_layout();
    assert_eq!(host.current(), host.pages()[0].id);
    assert_eq!(ui.client().focused_session(), Some(sid("s2")));
    assert_eq!(ui.client().focus(), Focus::Terminal);
}

#[test]
fn enter_attaches_the_selected_row() {
    let mut ui = status_page(120, 30);
    ui.key(KeyCode::Down).key(KeyCode::Down);
    let chosen = ui.client().status_selected().cloned();
    assert_eq!(chosen, Some(sid("s3")), "t28, then docs-arch, then f36");
    ui.key(KeyCode::Enter);
    ui.apply_layout_ops();
    assert_eq!(ui.client().selected(), chosen.as_ref());
    assert_eq!(ui.client().focused_session(), chosen);
}

#[test]
fn the_selected_row_is_highlighted() {
    let mut ui = status_page(120, 30);
    ui.key(KeyCode::Down).key(KeyCode::Down);
    let at = ui.at_in(table(&ui), "f36-cleanup-view");
    let other = ui.at_in(table(&ui), "docs-arch");
    assert_ne!(
        ui.buffer()[(at.x, at.y)].bg,
        ui.buffer()[(other.x, other.y)].bg
    );
    assert!(ui.buffer()[(at.x, at.y)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_click_on_the_selected_row_of_an_unfocused_status_pane_focuses_it_without_attaching() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 120, 20);
    let first = ui.client().focused_pane();
    ui.host_layout(LayoutOp::Split {
        pane: first,
        dir: SplitDir::Horizontal,
        ratio: Default::default(),
        content: Content::View(ViewKind::Status),
    });
    assert_eq!(ui.client().focused_pane(), first);
    // The table's selection is its first row: docs, the newest.
    assert_eq!(ui.client().status_selected(), Some(&sid("s2")));
    let at = ui.at_in(table(&ui), "docs");
    ui.click_at(at.x, at.y);
    assert_ne!(
        ui.client().focused_pane(),
        first,
        "the status pane is focused"
    );
    assert!(
        ui.layout_ops().is_empty(),
        "nothing attached: {:?}",
        ui.layout_ops()
    );
    assert_eq!(ui.client().focused_session(), None);
    // Now it is focused, a click on the selected row attaches it, to the pane showing the selection.
    ui.click_at(at.x, at.y);
    assert_eq!(ui.client().focused_pane(), first);
    assert_eq!(ui.client().focused_session(), Some(sid("s2")));
}

// ---------- scrolling ----------

/// `n` sessions just started (working, as far as the table can tell) `name-01` (oldest) to `name-<n>`, the status page open in a `cols` x `rows` window.
fn many(n: usize, cols: u16, rows: u16) -> Ui {
    let names: Vec<String> = (1..=n).map(|i| format!("name-{i:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut ui = Ui::with_sessions(&names, cols, rows);
    open_status(&mut ui);
    ui
}

#[test]
fn the_table_scrolls_to_keep_the_selection_in_view() {
    let mut ui = many(20, 100, 12);
    // Newest first: name-20 at the top.
    assert!(in_table(&ui, "name-20") && !in_table(&ui, "name-01"));
    for _ in 0..19 {
        ui.key(KeyCode::Down);
    }
    assert_eq!(ui.client().status_selected(), Some(&sid("s1")));
    assert!(in_table(&ui, "name-01") && !in_table(&ui, "name-20"));
    let at = ui.at_in(table(&ui), "name-01");
    assert!(matches!(
        ui.hit_at(at.x, at.y),
        Some(Target::StatusRow { session, .. }) if session == sid("s1")
    ));
    insta::assert_snapshot!(ui.region(pane(&ui)));
}

#[test]
fn the_wheel_scrolls_the_table_and_leaves_the_selection() {
    let mut ui = many(20, 100, 12);
    let at = ui.at_in(table(&ui), "name-18");
    ui.scroll_at(at.x, at.y, 5);
    assert!(!in_table(&ui, "name-20") && in_table(&ui, "name-15"));
    assert_eq!(ui.client().status_selected(), Some(&sid("s20")));
    ui.scroll_at(at.x, at.y, -10);
    assert!(in_table(&ui, "name-20"));
}

#[test]
fn page_down_moves_the_selection_a_page() {
    let mut ui = many(20, 100, 12);
    ui.key(KeyCode::PageDown);
    let selected = ui
        .client()
        .status_selected()
        .cloned()
        .unwrap_or_else(|| sid("none"));
    assert_ne!(selected, sid("s20"));
    assert_ne!(selected, sid("s19"), "more than one row");
    ui.at_in(table(&ui), &name_of(&ui, &selected));
}

// ---------- small and hostile ----------

#[test]
fn a_tiny_window_draws_what_fits_without_panicking() {
    let mut ui = status_page(120, 30);
    for (cols, rows) in [(1, 1), (10, 3), (30, 4), (40, 6), (61, 5), (70, 8)] {
        ui.resize(cols, rows);
        ui.key(KeyCode::Down).key(KeyCode::Char('g'));
        let _ = ui.screen();
    }
    ui.resize(70, 8);
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn hostile_names_are_drawn_display_safe() {
    let mut ui = status_page(120, 30);
    let evil = "evil\u{1b}[31m\u{202e}name\u{7}";
    ui.on_host(|h| {
        h.rename("s5", evil);
    });
    hook(
        &mut ui,
        "s4",
        "_shepherd/subagent/started",
        json!({ "id": "b1", "agent_type": "x" }),
    );
    let screen = ui.screen();
    assert!(
        !screen.contains('\u{1b}') && !screen.contains('\u{202e}') && !screen.contains('\u{7}')
    );
    // Escapes and controls dropped: the rest is drawn as text, in the table too.
    ui.at_in(table(&ui), "evil[31mname");
}

// ---------- machines ----------

fn peer(c: &mut ClientDriver, machine: &str, event: PeerEvent) -> Vec<ClientCommand> {
    c.handle(ClientEvent::Peer {
        machine: MachineId::new(machine),
        event,
    })
}

fn answer_peer(
    c: &mut ClientDriver,
    machine: &str,
    commands: &[ClientCommand],
    result: Value,
) -> Vec<ClientCommand> {
    let id = commands
        .iter()
        .find_map(|c| match c {
            ClientCommand::SendPeer {
                machine: to,
                message: ClientMessage::Request(r),
            } if to.as_str() == machine => Some(r.id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no request to {machine} in {commands:?}"));
    let response = RawResponse {
        id,
        result: Ok(result),
    };
    peer(c, machine, PeerEvent::Host(HostMessage::Response(response)))
}

/// Reaches peer `machine`, whose host's log is `host`'s.
fn reach(c: &mut ClientDriver, machine: &str, host: &HostDriver) {
    let tunnel = peer(
        c,
        machine,
        PeerEvent::Connected {
            at: ts("2026-10-29T12:00:00.000Z"),
        },
    );
    let hello = answer_peer(c, machine, &tunnel, json!({}));
    let ok = json!({ "version": PROTOCOL_VERSION, "machine": machine });
    let subscribe = answer_peer(c, machine, &hello, ok);
    for envelope in host.log().to_vec() {
        let entry = HostNotification::LogEntry(Box::new(LogEntry { envelope }));
        peer(
            c,
            machine,
            PeerEvent::Host(HostMessage::Notification(entry)),
        );
    }
    answer_peer(c, machine, &subscribe, json!({}));
}

/// This machine (`studio`) runs `api`; peer `laptop` runs `train`; peer `tower` ran `build` and can't be reached now.
pub(crate) fn three_machines(cols: u16, rows: u16) -> Ui {
    let mut c = ClientDriver::new();
    let subscribe = c.connect_named("studio");
    c.host().new_session("api");
    c.host().rename("api", "api");
    c.sync();
    let caught_up = c.answer(&subscribe, Ok(json!({})));
    let peers = json!({ "peers": [
        { "id": "m_laptop", "name": "laptop" },
        { "id": "m_tower", "name": "tower" },
    ] });
    c.answer(&caught_up, Ok(peers));
    for (machine, id) in [("m_laptop", "train"), ("m_tower", "build")] {
        let mut host = HostDriver::new();
        host.stamp_machine(machine);
        host.new_session(id);
        host.rename(id, id);
        reach(&mut c, machine, &host);
    }
    peer(&mut c, "m_tower", PeerEvent::Disconnected);
    peer(
        &mut c,
        "m_tower",
        PeerEvent::ConnectFailed {
            at: ts("2026-10-29T12:00:01.000Z"),
            error: "cannot reach tower".into(),
        },
    );
    let mut ui = Ui::new(c, cols, rows);
    open_status(&mut ui);
    ui
}

#[test]
fn every_machines_sessions_are_listed_and_an_offline_machines_are_dimmed() {
    let mut ui = three_machines(120, 20);
    ui.assert_contains("3 machines · 2 online");
    let dim = |ui: &Ui, name: &str| {
        let at = ui.at_in(table(ui), name);
        ui.buffer()[(at.x, at.y)].modifier.contains(Modifier::DIM)
    };
    assert!(dim(&ui, "build"), "tower's session is as last known");
    assert!(!dim(&ui, "train") && !dim(&ui, "api"));
    let offline = ui.region(row_of(&ui, "build"));
    assert!(offline.contains("offline"), "{offline}");
    ui.click("[g: by machine]");
    insta::assert_snapshot!(ui.region(pane(&ui)));
}
