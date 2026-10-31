//! The sidebar across machines (T31b): a header per machine above its sessions while there are peers, an unreachable
//! peer's group dimmed with its last-known sessions, and every row clickable.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use serde_json::{Value, json};
use shepherd_core::client::{ClientCommand, ClientEvent, Focus, Intent, PeerEvent};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    ClientMessage, HostMessage, HostNotification, LogEntry, PROTOCOL_VERSION, RawResponse,
};
use shepherd_term::Modes;
use shepherd_testkit::builders::{ClientDriver, HostDriver, ts};
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, render};

const WINDOW: Rect = Rect::new(0, 0, 80, 18);

fn m(id: &str) -> MachineId {
    MachineId::new(id)
}

fn peer(c: &mut ClientDriver, machine: &str, event: PeerEvent) -> Vec<ClientCommand> {
    c.handle(ClientEvent::Peer {
        machine: m(machine),
        event,
    })
}

/// Answers the one request to `machine` among `commands`.
fn answer(
    c: &mut ClientDriver,
    machine: &str,
    commands: &[ClientCommand],
    result: Value,
) -> Vec<ClientCommand> {
    let ids: Vec<_> = commands
        .iter()
        .filter_map(|c| match c {
            ClientCommand::SendPeer {
                machine: to,
                message: ClientMessage::Request(r),
            } if to.as_str() == machine => Some(r.id.clone()),
            _ => None,
        })
        .collect();
    let [id] = &ids[..] else {
        panic!("expected one request to {machine} in {commands:?}");
    };
    let response = RawResponse {
        id: id.clone(),
        result: Ok(result),
    };
    peer(c, machine, PeerEvent::Host(HostMessage::Response(response)))
}

/// Reaches peer `machine`, whose host's log is `host`'s.
fn reach(c: &mut ClientDriver, machine: &str, host: &HostDriver) {
    let ok = json!({ "version": PROTOCOL_VERSION, "machine": machine });
    reach_answering(c, machine, host, ok);
}

/// [`reach`], its host answering the hello with `ok`.
fn reach_answering(c: &mut ClientDriver, machine: &str, host: &HostDriver, ok: Value) {
    let at = ts("2026-10-29T12:00:00.000Z");
    let tunnel = peer(c, machine, PeerEvent::Connected { at });
    let hello = answer(c, machine, &tunnel, json!({}));
    let subscribe = answer(c, machine, &hello, ok);
    for envelope in host.log().to_vec() {
        let entry = HostNotification::LogEntry(Box::new(LogEntry { envelope }));
        peer(
            c,
            machine,
            PeerEvent::Host(HostMessage::Notification(entry)),
        );
    }
    answer(c, machine, &subscribe, json!({}));
}

/// A host machine called `name` with sessions `ids` (named after them), oldest first.
fn host_with(machine: &str, ids: &[&str]) -> HostDriver {
    let mut host = HostDriver::new();
    host.stamp_machine(machine);
    for id in ids {
        host.new_session(id);
        host.rename(id, id);
    }
    host
}

/// This machine, `studio`, runs `docs` and `api` (newest); peer `laptop` is reached and runs `train`; peer `tower`
/// ran `build` but can no longer be reached. Sized for `window`.
fn three_machines(window: Rect) -> ClientDriver {
    let mut c = ClientDriver::new();
    let subscribe = c.connect_named("studio");
    three_machines_from(c, subscribe, window, |_| {}, |_| {}, json!({}))
}

/// [`three_machines`] from `c`, connected to its host with `subscribe` asked, `local` and `laptop` run on each host
/// after its sessions are made, and the laptop's hello answer also saying `laptop_hello`.
fn three_machines_from(
    mut c: ClientDriver,
    subscribe: Vec<ClientCommand>,
    window: Rect,
    local: impl FnOnce(&mut HostDriver),
    laptop: impl FnOnce(&mut HostDriver),
    laptop_hello: Value,
) -> ClientDriver {
    for id in ["docs", "api"] {
        c.host().new_session(id);
        c.host().rename(id, id);
    }
    local(c.host());
    c.sync();
    let caught_up = c.answer(&subscribe, Ok(json!({})));
    let peers = json!({ "peers": [
        { "id": "m_laptop", "name": "laptop" },
        { "id": "m_tower", "name": "tower" },
    ] });
    c.answer(&caught_up, Ok(peers));
    let mut laptop_host = HostDriver::new();
    laptop_host.stamp_machine("m_laptop");
    laptop_host.new_session("train");
    laptop_host.rename("train", "train");
    laptop(&mut laptop_host);
    let mut ok = json!({ "version": PROTOCOL_VERSION, "machine": "m_laptop" });
    if let (Some(ok), Some(extra)) = (ok.as_object_mut(), laptop_hello.as_object()) {
        ok.extend(extra.clone());
    }
    reach_answering(&mut c, "m_laptop", &laptop_host, ok);
    reach(&mut c, "m_tower", &host_with("m_tower", &["build"]));
    peer(&mut c, "m_tower", PeerEvent::Disconnected);
    peer(
        &mut c,
        "m_tower",
        PeerEvent::ConnectFailed {
            at: ts("2026-10-29T12:00:01.000Z"),
            error: "cannot reach tower".into(),
        },
    );
    let resize = Event::Resize(window.width, window.height);
    for intent in map_event(
        &resize,
        c.client(),
        &Modes::default(),
        window,
        &mut Pointer::default(),
    ) {
        c.user(intent);
    }
    c
}

fn buffer(c: &ClientDriver, window: Rect) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(window.width, window.height)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(c.client(), &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

/// Where `text` starts in `buf`, reading cell by cell.
fn at(buf: &Buffer, text: &str) -> Position {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    buf.area
        .positions()
        .find(|p| {
            chars.iter().enumerate().all(|(i, ch)| {
                let x = p.x + i as u16;
                x < buf.area.right() && buf[(x, p.y)].symbol() == ch
            })
        })
        .unwrap_or_else(|| panic!("{text:?} not drawn: {buf:?}"))
}

fn click(c: &ClientDriver, window: Rect, p: Position, button: MouseButton) -> Vec<Intent> {
    let mut pointer = Pointer::default();
    let mut intents = Vec::new();
    for kind in [MouseEventKind::Down(button), MouseEventKind::Up(button)] {
        let event = Event::Mouse(MouseEvent {
            kind,
            column: p.x,
            row: p.y,
            modifiers: KeyModifiers::NONE,
        });
        intents.extend(map_event(
            &event,
            c.client(),
            &Modes::default(),
            window,
            &mut pointer,
        ));
    }
    intents
}

#[test]
fn sidebar_grouped_by_machine() {
    let c = three_machines(WINDOW);
    insta::assert_debug_snapshot!(buffer(&c, WINDOW));
}

#[test]
fn an_unreachable_machines_group_is_dimmed() {
    let c = three_machines(WINDOW);
    let buf = buffer(&c, WINDOW);
    let dim = |text: &str| {
        let p = at(&buf, text);
        buf[(p.x, p.y)].modifier.contains(Modifier::DIM)
    };
    assert!(dim("tower"), "the header");
    assert!(dim("build"), "its session");
    assert!(!dim("laptop"));
    assert!(!dim("train"));
}

#[test]
fn clicking_a_machines_header_means_that_machine() {
    let c = three_machines(WINDOW);
    let buf = buffer(&c, WINDOW);
    let header = at(&buf, "tower");
    assert_eq!(
        hit(&layout(WINDOW), c.client(), header.x, header.y),
        Some(Target::Machine(m("m_tower")))
    );
    // Either of its lines.
    assert_eq!(
        hit(&layout(WINDOW), c.client(), header.x, header.y + 1),
        Some(Target::Machine(m("m_tower")))
    );
    assert_eq!(
        click(&c, WINDOW, header, MouseButton::Left),
        [Intent::SelectMachine(m("m_tower"))]
    );
    assert_eq!(click(&c, WINDOW, header, MouseButton::Right), []);
}

#[test]
fn a_peers_session_row_selects_it_and_its_close_button_closes_it() {
    let c = three_machines(WINDOW);
    let buf = buffer(&c, WINDOW);
    let row = at(&buf, "train");
    assert_eq!(
        click(&c, WINDOW, row, MouseButton::Left),
        [
            Intent::SelectSession(SessionId::new("train")),
            Intent::SetFocus(Focus::Terminal)
        ]
    );
    let close = Position::new(22, row.y);
    assert_eq!(buf[(close.x, close.y)].symbol(), "×");
    assert_eq!(
        hit(&layout(WINDOW), c.client(), close.x, close.y),
        Some(Target::CloseSession(SessionId::new("train")))
    );
}

#[test]
fn the_collapsed_sidebar_marks_each_machine_and_its_header_is_clickable() {
    let narrow = Rect::new(0, 0, 40, 18);
    let c = three_machines(narrow);
    let buf = buffer(&c, narrow);
    // The first header sits right under the `+`.
    assert_eq!(buf[(1, 3)].symbol(), "≡");
    assert_eq!(
        hit(&layout(narrow), c.client(), 1, 3),
        Some(Target::Machine(m("m_test")))
    );
}

#[test]
fn headers_take_rows_of_a_short_sidebar_and_the_selection_stays_in_view() {
    // Room for two entries of two lines: headers count.
    let short = Rect::new(0, 0, 80, 8);
    let mut c = three_machines(short);
    c.user(Intent::SelectSession(SessionId::new("build")));
    let buf = buffer(&c, short);
    let rows: Vec<String> = (3..7)
        .map(|y| (0..23).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect();
    assert!(rows[0].contains("tower"), "{rows:#?}");
    assert!(rows[2].contains("build"), "{rows:#?}");
}

#[test]
fn the_grouped_sidebar_draws_in_any_window_however_small() {
    let c = three_machines(WINDOW);
    for cols in 0..30 {
        for rows in 0..12 {
            let window = Rect::new(0, 0, cols, rows);
            let buf = buffer(&c, window);
            for p in window.positions() {
                let _ = hit(&layout(window), c.client(), p.x, p.y);
            }
            assert_eq!(buf.area, window);
        }
    }
}

// ---------- choosing where a new session runs (T32) ----------

/// [`three_machines`] in `window`, with profiles and workspaces: this machine offers `claude` (the default) and `shell`
/// in workspace `/here/app`, the laptop offers `codex` in `/there/api` after `/there/old`.
fn with_profiles(window: Rect) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.handle(ClientEvent::Start);
    let hello = c.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    let ok = json!({
        "version": PROTOCOL_VERSION,
        "machine": "m_test",
        "name": "studio",
        "profiles": [
            { "name": "claude", "agent": "claude-code" },
            { "name": "shell", "agent": "shell" },
        ],
        "default_profile": "claude",
    });
    let subscribe = c.answer(&hello, Ok(ok));
    let laptop_hello = json!({
        "profiles": [{ "name": "codex", "agent": "codex" }],
        "default_profile": "codex",
    });
    three_machines_from(
        c,
        subscribe,
        window,
        |host| {
            host.select_workspace("/here/app");
        },
        |host| {
            host.select_workspace("/there/old");
            host.select_workspace("/there/api");
        },
        laptop_hello,
    )
}

const POPUP_WINDOW: Rect = Rect::new(0, 0, 80, 24);

#[test]
fn new_session_popup_with_a_machine_selector() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("api")));
    c.user(Intent::BeginNewSession);
    insta::assert_debug_snapshot!(buffer(&c, POPUP_WINDOW));
}

#[test]
fn new_session_popup_on_the_laptop_offers_its_profiles() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("train")));
    c.user(Intent::BeginNewSession);
    insta::assert_debug_snapshot!(buffer(&c, POPUP_WINDOW));
}

#[test]
fn clicking_a_machine_in_the_new_session_popup_chooses_it() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("api")));
    c.user(Intent::BeginNewSession);
    let buf = buffer(&c, POPUP_WINDOW);
    let laptop = at(&buf, "○ laptop");
    assert_eq!(
        hit(&layout(POPUP_WINDOW), c.client(), laptop.x, laptop.y),
        Some(Target::MachineRow(m("m_laptop")))
    );
    assert_eq!(
        click(&c, POPUP_WINDOW, laptop, MouseButton::Left),
        [Intent::PickMachine(m("m_laptop"))]
    );
    // The whole line is the row.
    assert_eq!(
        hit(&layout(POPUP_WINDOW), c.client(), laptop.x + 20, laptop.y),
        Some(Target::MachineRow(m("m_laptop")))
    );
}

#[test]
fn an_unreachable_machine_is_drawn_dim_with_why_and_isnt_clickable() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::BeginNewSession);
    let buf = buffer(&c, POPUP_WINDOW);
    let tower = at(&buf, "○ tower (offline: cannot reach tower)");
    assert!(buf[(tower.x, tower.y)].modifier.contains(Modifier::DIM));
    assert_eq!(
        hit(&layout(POPUP_WINDOW), c.client(), tower.x + 2, tower.y),
        Some(Target::Popup)
    );
    assert_eq!(click(&c, POPUP_WINDOW, tower, MouseButton::Left), []);
}

#[test]
fn the_new_session_popups_start_button_confirms_and_its_profile_rows_pick() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::BeginNewSession);
    let buf = buffer(&c, POPUP_WINDOW);
    let start = at(&buf, "[Start]");
    assert_eq!(
        click(&c, POPUP_WINDOW, start, MouseButton::Left),
        [Intent::Confirm]
    );
    let shell = at(&buf, "shell");
    assert_eq!(
        click(&c, POPUP_WINDOW, shell, MouseButton::Left),
        [Intent::PickProfile("shell".into())]
    );
}

#[test]
fn the_wheel_over_the_machines_scrolls_the_profiles() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::BeginNewSession);
    let buf = buffer(&c, POPUP_WINDOW);
    let laptop = at(&buf, "○ laptop");
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: laptop.x,
        row: laptop.y,
        modifiers: KeyModifiers::NONE,
    });
    let intents = map_event(
        &event,
        c.client(),
        &Modes::default(),
        POPUP_WINDOW,
        &mut Pointer::default(),
    );
    assert!(
        matches!(intents[..], [Intent::ScrollPicker { by: 1, .. }]),
        "{intents:?}"
    );
}

#[test]
fn workspace_switcher_with_a_machine_selector() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("train")));
    c.user(Intent::BeginWorkspace);
    insta::assert_debug_snapshot!(buffer(&c, POPUP_WINDOW));
}

#[test]
fn clicking_a_machine_in_the_switcher_chooses_it() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("train")));
    c.user(Intent::BeginWorkspace);
    let buf = buffer(&c, POPUP_WINDOW);
    let studio = at(&buf, "○ studio");
    assert_eq!(
        click(&c, POPUP_WINDOW, studio, MouseButton::Left),
        [Intent::PickMachine(m("m_test"))]
    );
    let row = at(&buf, "/there/old");
    assert_eq!(
        click(&c, POPUP_WINDOW, row, MouseButton::Left),
        [Intent::PickWorkspace("/there/old".into())]
    );
}

#[test]
fn the_sidebar_shows_the_selected_sessions_machines_workspace() {
    let mut c = with_profiles(POPUP_WINDOW);
    c.user(Intent::SelectSession(SessionId::new("train")));
    at(&buffer(&c, POPUP_WINDOW), "⌂ api");
    c.user(Intent::SelectSession(SessionId::new("api")));
    at(&buffer(&c, POPUP_WINDOW), "⌂ app");
}

#[test]
fn the_popups_with_a_machine_selector_draw_in_any_window_however_small() {
    for open in [Intent::BeginNewSession, Intent::BeginWorkspace] {
        let mut c = with_profiles(POPUP_WINDOW);
        c.user(open);
        for cols in 0..44 {
            for rows in 0..15 {
                let window = Rect::new(0, 0, cols, rows);
                let buf = buffer(&c, window);
                // Every column of every other line.
                for p in window.positions().filter(|p| p.y % 2 == 0) {
                    let _ = hit(&layout(window), c.client(), p.x, p.y);
                }
                assert_eq!(buf.area, window);
            }
        }
    }
}
