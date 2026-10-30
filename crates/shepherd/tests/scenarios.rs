//! Scenario tests: user flows across a host and client runtime, wired with testkit fakes.
//! One scenario per user-visible feature. Edge cases belong in core tests.

mod support;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use shepherd::client::{
    ClientError, ClientRuntime, Connector, EventSender, MAX_BATCH, TerminalView, View,
    forward_signals,
};
use shepherd::terminal::{TerminalSetup, with_terminal};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{
    Client, Connection, HELLO_TIMEOUT, Intent, MAX_CONNECT_ATTEMPTS, NOTICE_TIMEOUT, Popup,
    RETRY_INTERVAL,
};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::LaunchError;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{BuildId, ExeDigest, SessionNew, UpgradeAnswer};
use shepherd_term::{PANIC_TRIGGER, Screen};
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use shepherd_tui::{Screens, layout, pane_size};
use support::fake_host::FakeHost;
use support::fake_terminal::FakeTerminal;

/// A host slot the fake launcher fills.
type HostSlot = Arc<Mutex<Option<FakeHost>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A client runtime on a fake network whose launcher starts a [`FakeHost`], with a clock the test moves.
struct Harness {
    net: FakeNetwork,
    host: HostSlot,
    launcher: FakeHostLauncher,
    now: Arc<Mutex<jiff::Timestamp>>,
    runtime: ClientRuntime<FakeLink, Connector<FakeLink>, FakeHostLauncher>,
}

impl Harness {
    fn new() -> Self {
        Self::with_launcher(|net, host| {
            FakeHostLauncher::new().on_launch(move || {
                *lock(&host) = Some(FakeHost::start(&net));
            })
        })
    }

    fn with_launcher(make: impl FnOnce(FakeNetwork, HostSlot) -> FakeHostLauncher) -> Self {
        let net = FakeNetwork::new();
        let host: HostSlot = Arc::default();
        let launcher = make(net.clone(), host.clone());
        let now = Arc::new(Mutex::new(
            "2026-10-29T12:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap_or_else(|e| panic!("{e}")),
        ));
        let runtime = client_runtime(&net, &launcher, &now);
        Self {
            net,
            host,
            launcher,
            now,
            runtime,
        }
    }

    /// A second client on the same network and host, as starting `shepherd` again would make.
    fn reattach(&self) -> Self {
        Self {
            net: self.net.clone(),
            host: self.host.clone(),
            launcher: self.launcher.clone(),
            now: self.now.clone(),
            runtime: client_runtime(&self.net, &self.launcher, &self.now),
        }
    }

    /// Pumps host and client until neither has anything left to do.
    fn settle(&mut self) {
        loop {
            let host = lock(&self.host).as_mut().map_or(0, FakeHost::pump);
            if host + self.runtime.pump() == 0 {
                return;
            }
        }
    }

    /// Moves the clock past the retry interval and ticks.
    fn wait_retry(&mut self) {
        self.advance(RETRY_INTERVAL);
        self.runtime.tick();
        self.settle();
    }

    fn advance(&mut self, by: jiff::SignedDuration) {
        let mut now = lock(&self.now);
        *now = now.checked_add(by).unwrap_or_else(|e| panic!("{e}"));
    }

    fn user(&mut self, intent: Intent) {
        self.runtime.user(intent);
        self.settle();
    }

    fn selected(&self) -> Option<String> {
        self.runtime.client().selected().map(|s| s.to_string())
    }

    fn screen(&self, session: &str) -> String {
        self.runtime
            .screen(&SessionId::new(session))
            .map(first_line)
            .unwrap_or_default()
    }

    fn inputs(&self) -> Vec<(String, String)> {
        lock(&self.host)
            .as_ref()
            .map(|h| {
                h.inputs()
                    .iter()
                    .map(|(s, b)| (s.to_string(), String::from_utf8_lossy(b).into_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn client_runtime(
    net: &FakeNetwork,
    launcher: &FakeHostLauncher,
    now: &Arc<Mutex<jiff::Timestamp>>,
) -> ClientRuntime<FakeLink, Connector<FakeLink>, FakeHostLauncher> {
    let clock = {
        let now = now.clone();
        move || Timestamp::parse(lock(&now).to_string()).unwrap_or_else(|e| panic!("{e}"))
    };
    let connect_net = net.clone();
    let connect: Connector<FakeLink> =
        Box::new(move |sink| connect_net.connect(sink).map_err(|e| e.to_string()));
    ClientRuntime::new(MachineId::new("m_client"), connect, launcher.clone(), clock)
}

fn first_line(screen: &Screen) -> String {
    screen
        .row(0)
        .map(|c| c.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[test]
fn two_sessions_switching_between_them_routes_input_to_the_selected_one() {
    let mut h = Harness::new();
    // No host is running: the first connect fails, and the client launches one.
    h.runtime.start();
    h.settle();
    assert_eq!(h.launcher.launches(), 1);
    h.wait_retry();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);

    h.user(Intent::NewSession);
    h.user(Intent::NewSession);
    assert_eq!(h.runtime.client().sessions().len(), 2);
    assert_eq!(
        h.selected().as_deref(),
        Some("s2"),
        "a new session is selected"
    );

    h.user(Intent::keys(b"two".to_vec()));
    h.user(Intent::NextSession);
    assert_eq!(h.selected().as_deref(), Some("s1"));
    h.user(Intent::keys(b"one".to_vec()));

    assert_eq!(
        h.inputs(),
        [("s2".into(), "two".into()), ("s1".into(), "one".into())]
    );
    // Each session's echo lands on its own screen.
    assert_eq!(
        (h.screen("s1"), h.screen("s2")),
        ("one".into(), "two".into())
    );
}

#[test]
fn a_host_that_cannot_be_started_is_a_clear_error() {
    let mut h = Harness::with_launcher(|_, _| {
        FakeHostLauncher::new().failing(LaunchError::SpawnFailed("no such file".into()))
    });
    let Err(ClientError::Fatal(why)) = h.runtime.run() else {
        panic!("run should fail");
    };
    assert!(why.contains("could not start the shepherd host"), "{why}");
    assert!(why.contains("no such file"), "{why}");
}

#[test]
fn a_host_that_reports_why_it_cannot_start_is_given_up_on_with_its_message() {
    let message =
        "shepherd host: /home/me/.shepherd has mode 755. Fix it with: chmod 700 /home/me/.shepherd";
    let mut h = Harness::with_launcher(|_, _| {
        FakeHostLauncher::new().failing(LaunchError::HostFailed(message.into()))
    });
    let terminal = FakeTerminal::enhanced();
    let mut view = test_view();
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    let Ok(Err(ClientError::Fatal(why))) = result else {
        panic!("run should fail: {result:?}");
    };
    assert!(why.contains(message), "{why}");
    assert_eq!(terminal.calls(), RESTORED);
    assert_eq!(h.launcher.launches(), 0);
}

#[test]
fn a_host_that_never_listens_is_given_up_on_after_the_last_attempt() {
    // The launch "succeeds" but no host ever listens.
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    h.runtime.start();
    h.settle();
    for _ in 1..MAX_CONNECT_ATTEMPTS {
        assert!(!h.runtime.done());
        h.wait_retry();
    }
    assert!(h.runtime.done());
    assert_eq!(h.launcher.launches(), 1);
    let why = h.runtime.client().fatal().unwrap_or_default();
    assert!(
        why.contains("could not connect to the shepherd host"),
        "{why}"
    );
}

#[test]
fn a_host_slow_to_answer_hello_is_retried_and_the_old_links_events_are_ignored() {
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    *lock(&h.host) = Some(FakeHost::start(&h.net));
    // Connect, but the host doesn't get to answer the hello before it times out.
    h.runtime.start();
    h.runtime.pump();
    h.advance(HELLO_TIMEOUT);
    h.runtime.tick();
    assert!(matches!(
        h.runtime.client().connection(),
        Connection::Waiting { .. }
    ));
    // The retry replaces the first link. Dropping it queues that link's `Disconnected` behind the new
    // connection, where it must not be taken for the new link going away.
    h.advance(RETRY_INTERVAL);
    h.runtime.tick();
    h.settle();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    h.user(Intent::NewSession);
    h.user(Intent::keys(b"hi".to_vec()));
    assert_eq!(h.inputs(), [("s1".into(), "hi".into())]);
}

// ---------- the TUI ----------

const WINDOW: (u16, u16) = (100, 30);

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn test_view() -> TerminalView<TestBackend> {
    let backend =
        ratatui::Terminal::new(TestBackend::new(WINDOW.0, WINDOW.1)).unwrap_or_else(|e| match e {});
    TerminalView::new(backend)
}

/// The text of columns `x` of row `y` in the view's last frame, trailing blanks trimmed.
fn text(view: &TerminalView<TestBackend>, x: std::ops::Range<u16>, y: u16) -> String {
    let buf = view.terminal().backend().buffer();
    x.map(|x| buf[(x, y)].symbol().to_owned())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

impl Harness {
    /// Started, with the host launched and the hello answered, in a [`WINDOW`]-sized terminal.
    fn ready() -> Self {
        let mut h = Self::new();
        h.runtime.start();
        h.settle();
        h.wait_retry();
        assert_eq!(h.runtime.client().connection(), &Connection::Ready);
        h.term(Event::Resize(WINDOW.0, WINDOW.1));
        h
    }

    fn term(&mut self, event: Event) {
        assert!(self.runtime.events().terminal(event));
        self.settle();
    }

    fn keys(&mut self, keys: &[KeyEvent]) {
        for key in keys {
            self.term(Event::Key(*key));
        }
    }

    fn typed(&mut self, text: &str) {
        for c in text.chars() {
            self.term(Event::Key(ch(c)));
        }
    }

    fn draw(&mut self, view: &mut TerminalView<TestBackend>) {
        view.draw(self.runtime.client(), self.runtime.screens())
            .unwrap_or_else(|e| panic!("{e}"));
    }

    fn host<R>(&self, f: impl FnOnce(&FakeHost) -> R) -> R {
        f(lock(&self.host)
            .as_ref()
            .unwrap_or_else(|| panic!("no host")))
    }

    /// Everything typed into `session`, joined.
    fn typed_into(&self, session: &str) -> String {
        self.inputs()
            .into_iter()
            .filter(|(s, _)| s == session)
            .map(|(_, t)| t)
            .collect()
    }
}

#[test]
fn keys_create_and_switch_sessions_and_the_screen_shows_the_selected_one() {
    let mut h = Harness::ready();
    let mut view = test_view();
    let pane = layout(Rect::new(0, 0, WINDOW.0, WINDOW.1)).pane;
    // Left of each row's close button.
    let side = 0..pane.x - 2;
    let screen = pane.x..pane.right();

    h.keys(&[ctrl('b'), ch('n'), ctrl('b'), ch('n')]);
    h.typed("hello");
    h.keys(&[ctrl('b'), ch('j')]);
    h.typed("world");
    h.draw(&mut view);

    assert_eq!(
        (h.typed_into("s2"), h.typed_into("s1")),
        ("hello".into(), "world".into())
    );
    assert_eq!(text(&view, screen.clone(), 0), "world");
    // Newest first, under the two header lines: s2 (`shell-2`) on top, s1 below it and selected.
    let buf = view.terminal().backend().buffer().clone();
    assert!(text(&view, side.clone(), 2).ends_with("shell-2"), "{buf:?}");
    assert!(text(&view, side.clone(), 4).ends_with("shell-1"), "{buf:?}");
    let highlighted = |y: u16| buf[(2, y)].bg != ratatui::style::Color::Reset;
    assert_eq!((highlighted(2), highlighted(4)), (false, true));

    h.keys(&[ctrl('b'), ch('k')]);
    h.draw(&mut view);
    assert_eq!(text(&view, screen, 0), "hello");
}

#[test]
fn the_selected_session_is_resized_to_the_pane_not_the_window() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    // The session prints before the resize, so the client already has its screen.
    h.typed("x");
    assert!(h.runtime.screen(&SessionId::new("s1")).is_some());
    h.term(Event::Resize(120, 40));
    let pane = pane_size(120, 40);
    let last = h.host(|host| host.resizes().last().cloned());
    let last = last.unwrap_or_else(|| panic!("no resize sent"));
    assert_eq!(
        (last.session.as_str(), last.cols, last.rows),
        ("s1", pane.cols, pane.rows)
    );
    // The client's own screen of the session is resized to match.
    let screen = h.runtime.screen(&SessionId::new("s1")).map(Screen::size);
    assert_eq!(screen, Some((pane.cols, pane.rows)));
}

#[test]
fn a_session_whose_screen_panics_gets_a_blank_one_of_the_same_size_and_the_client_keeps_going() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    h.term(Event::Resize(120, 40));
    h.typed("before");
    let s1 = SessionId::new("s1");
    let size = h.runtime.screen(&s1).map(Screen::size);
    // The fake host echoes input, so this is fed to the client's screen of s1.
    h.user(Intent::keys(PANIC_TRIGGER.to_vec()));
    assert!(!h.runtime.done());
    assert_eq!(h.runtime.screen(&s1).map(Screen::size), size);
    assert_eq!(h.screen("s1"), "");
    h.typed("after");
    assert_eq!(h.screen("s1"), "after");
}

#[test]
fn a_flood_of_output_is_drawn_once() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    // 50 echoes queue up in the client before it gets to run.
    for _ in 0..50 {
        h.runtime.user(Intent::keys(b"x".to_vec()));
    }
    lock(&h.host).as_mut().map(FakeHost::pump);
    let mut view = CountingView::default();
    let drew = h.runtime.pump_frame(&mut view);
    assert!(matches!(drew, Ok(true)), "{drew:?}");
    assert_eq!(view.draws, 1);
    assert_eq!(h.screen("s1"), "x".repeat(50));
    // Nothing new: no redraw.
    let drew = h.runtime.pump_frame(&mut view);
    assert!(matches!(drew, Ok(false)), "{drew:?}");
    assert_eq!(view.draws, 1);
}

#[test]
fn the_running_loop_draws_queued_input_as_one_batch() {
    /// Asks the client to quit at its second draw.
    struct QuitOnSecondDraw {
        draws: usize,
        events: EventSender,
    }
    impl View for QuitOnSecondDraw {
        fn size(&mut self) -> std::io::Result<(u16, u16)> {
            Ok(WINDOW)
        }
        fn draw(&mut self, _: &Client, _: &Screens) -> std::io::Result<()> {
            self.draws += 1;
            if self.draws == 2 {
                self.events.shutdown();
            }
            Ok(())
        }
    }
    // No host answers; the keys just need handling.
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    let events = h.runtime.events();
    for _ in 0..50 {
        events.terminal(Event::Key(ch('x')));
    }
    let mut view = QuitOnSecondDraw { draws: 0, events };
    let result = h.runtime.run_with(&mut view);
    assert_eq!(result, Ok(()));
    // The first frame, then all 50 keys in one.
    assert_eq!(view.draws, 2);
}

#[derive(Default)]
struct CountingView {
    draws: usize,
}

impl View for CountingView {
    fn size(&mut self) -> std::io::Result<(u16, u16)> {
        Ok(WINDOW)
    }

    fn draw(&mut self, _: &Client, _: &Screens) -> std::io::Result<()> {
        self.draws += 1;
        Ok(())
    }
}

#[test]
fn mouse_goes_to_a_session_that_turned_mouse_mode_on() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);

    // The fake host echoes input, so this turns on SGR mouse reporting in the session's terminal.
    h.user(Intent::keys(b"\x1b[?1000h\x1b[?1006h".to_vec()));

    let pane = layout(Rect::new(0, 0, WINDOW.0, WINDOW.1)).pane;
    h.term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: pane.x + 2,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    let last = h.inputs().last().cloned();
    assert_eq!(last, Some(("s1".into(), "\x1b[<64;3;2M".into())));
}

#[test]
fn q_detaches_and_starting_again_reattaches_to_the_running_sessions() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n'), ctrl('b'), ch('n')]);
    h.keys(&[ctrl('b'), ch('q')]);
    assert!(h.runtime.done());
    assert_eq!(h.host(FakeHost::sessions).len(), 2, "sessions keep running");

    let mut again = h.reattach();
    again.runtime.start();
    again.settle();
    assert_eq!(again.runtime.client().connection(), &Connection::Ready);
    assert_eq!(again.runtime.client().sessions().len(), 2);
}

/// Where `label` is drawn in the view's last frame.
fn find(view: &TerminalView<TestBackend>, label: &str) -> (u16, u16) {
    let buf = view.terminal().backend().buffer();
    let chars: Vec<String> = label.chars().map(String::from).collect();
    let found = buf.area.positions().find(|p| {
        chars.iter().enumerate().all(|(i, c)| {
            let x = p.x + i as u16;
            x < buf.area.right() && buf[(x, p.y)].symbol() == c
        })
    });
    let p = found.unwrap_or_else(|| panic!("{label:?} not drawn: {buf:?}"));
    (p.x, p.y)
}

impl Harness {
    /// A left press and release at `(col, row)`.
    fn click(&mut self, (column, row): (u16, u16)) {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            self.term(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }));
        }
    }
}

#[test]
fn clicks_create_sessions_and_pick_the_one_that_gets_typing() {
    let mut h = Harness::ready();
    let mut view = test_view();
    h.draw(&mut view);
    let button = find(&view, "+ New session");
    h.click(button);
    h.click(button);
    h.draw(&mut view);
    assert_eq!(h.host(FakeHost::sessions).len(), 2);

    // Newest first: shell-2 (s2) is the first row, shell-1 (s1) the second.
    let first = find(&view, "shell-2");
    let second = find(&view, "shell-1");
    h.click(second);
    h.typed("one");
    h.click(first);
    h.typed("two");
    assert_eq!(
        (h.typed_into("s1"), h.typed_into("s2")),
        ("one".into(), "two".into())
    );
}

#[test]
fn clicking_new_session_then_a_profile_starts_a_session_with_that_profile() {
    let mut h = Harness::with_launcher(|net, host| {
        FakeHostLauncher::new().on_launch(move || {
            let profiles = [
                ("claude", AgentKind::ClaudeCode),
                ("shell", AgentKind::Shell),
            ];
            *lock(&host) = Some(FakeHost::start(&net).with_profiles(&profiles));
        })
    });
    h.runtime.start();
    h.settle();
    h.wait_retry();
    h.term(Event::Resize(WINDOW.0, WINDOW.1));
    let mut view = test_view();
    h.draw(&mut view);
    h.click(find(&view, "+ New session"));
    h.draw(&mut view);
    h.click(find(&view, "shell "));
    let news = h.host(|host| host.session_news().to_vec());
    assert_eq!(
        news,
        [SessionNew {
            profile: Some("shell".to_owned()),
            cwd: None,
        }]
    );
    assert_eq!(h.host(FakeHost::sessions).len(), 1);
}

impl Harness {
    /// A right press and release at `(col, row)`.
    fn right_click(&mut self, (column, row): (u16, u16)) {
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
        ] {
            self.term(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }));
        }
    }
}

/// Where the `×` of the row whose first line is `y` is drawn: the last one on that line of the sidebar.
fn close_button(view: &TerminalView<TestBackend>, y: u16) -> (u16, u16) {
    let buf = view.terminal().backend().buffer();
    let side = layout(buf.area).sidebar;
    let x = (side.x..side.right())
        .rev()
        .find(|&x| buf[(x, y)].symbol() == "×");
    let x = x.unwrap_or_else(|| panic!("no close button on row {y}: {buf:?}"));
    (x, y)
}

#[test]
fn the_mouse_renames_a_session_from_its_menu_and_closes_one_by_its_close_button() {
    let mut h = Harness::ready();
    let mut view = test_view();
    h.keys(&[ctrl('b'), ch('n'), ctrl('b'), ch('n')]);
    h.draw(&mut view);

    // Rename the older session, shell-1, from its context menu.
    h.right_click(find(&view, "shell-1"));
    h.draw(&mut view);
    h.click(find(&view, "Rename"));
    h.keys(&[ctrl('u')]);
    h.typed("api");
    h.draw(&mut view);
    h.click(find(&view, "[Save]"));
    h.draw(&mut view);
    let (_, api) = find(&view, "api");
    assert_eq!(h.runtime.client().popup(), None);

    // Close it by its ×, then [Close].
    h.click(close_button(&view, api));
    h.draw(&mut view);
    h.click(find(&view, "[Close]"));
    h.draw(&mut view);
    assert_eq!(h.host(FakeHost::sessions), [SessionId::new("s2")]);
    let names: Vec<&str> = (h.runtime.client().sessions().iter())
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["shell-2"]);
}

#[test]
fn mouse_follows_the_layout_after_the_window_crosses_the_collapse_width() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    h.user(Intent::keys(b"\x1b[?1000h\x1b[?1006h".to_vec()));
    // Narrow enough to collapse the sidebar: the pane moves left.
    h.term(Event::Resize(50, 30));
    let pane = layout(Rect::new(0, 0, 50, 30)).pane;
    h.term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.x + 2,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    let last = h.inputs().last().cloned();
    assert_eq!(last, Some(("s1".into(), "\x1b[<0;3;2M".into())));
}

#[test]
fn shift_enter_reaches_a_kitty_session_as_its_own_key_and_a_legacy_one_as_alt_enter() {
    let mut h = Harness::ready();
    let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    h.keys(&[ctrl('b'), ch('n')]);
    // The fake host echoes input, so this pushes kitty keyboard flag 1 in s1's terminal.
    h.user(Intent::keys(b"\x1b[>1u".to_vec()));
    h.keys(&[shift_enter]);
    assert_eq!(
        h.inputs().last().cloned(),
        Some(("s1".into(), "\x1b[13;2u".into()))
    );

    h.keys(&[ctrl('b'), ch('n'), shift_enter]);
    assert_eq!(
        h.inputs().last().cloned(),
        Some(("s2".into(), "\x1b\r".into()))
    );
}

#[test]
fn ctrl_backslash_from_a_legacy_outer_terminal_reaches_the_session() {
    let mut h = Harness::ready();
    // What crossterm reports for byte 0x1C when the outer terminal doesn't report keys unambiguously.
    let reported = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
    h.keys(&[ctrl('b'), ch('n'), reported]);
    assert_eq!(
        h.inputs().last().cloned(),
        Some(("s1".into(), "\x1c".into()))
    );
    h.user(Intent::keys(b"\x1b[>1u".to_vec()));
    h.keys(&[reported]);
    assert_eq!(
        h.inputs().last().cloned(),
        Some(("s1".into(), "\x1b[92;5u".into()))
    );
}

#[test]
fn ctrl_and_a_digit_from_an_enhanced_outer_terminal_stay_a_digit() {
    let mut h = Harness::ready();
    h.runtime.set_keyboard_enhanced(true);
    let ctrl_4 = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL);
    h.keys(&[ctrl('b'), ch('n')]);
    h.user(Intent::keys(b"\x1b[>1u".to_vec()));
    h.keys(&[ctrl_4]);
    assert_eq!(
        h.inputs().last().cloned(),
        Some(("s1".into(), "\x1b[52;5u".into()))
    );
}

#[test]
fn a_tick_that_changes_the_connection_redraws() {
    /// Moves the clock past the retry at its first draw, records the connect attempt at each draw, and quits at
    /// the second.
    struct Recorder {
        now: Arc<Mutex<jiff::Timestamp>>,
        events: EventSender,
        attempts: Vec<u32>,
    }
    impl View for Recorder {
        fn size(&mut self) -> std::io::Result<(u16, u16)> {
            Ok(WINDOW)
        }
        fn draw(&mut self, client: &Client, _: &Screens) -> std::io::Result<()> {
            let attempt = match client.connection() {
                Connection::Waiting { attempt, .. } | Connection::Connecting { attempt } => {
                    *attempt
                }
                _ => 0,
            };
            self.attempts.push(attempt);
            if self.attempts.len() == 1 {
                let mut now = lock(&self.now);
                *now = now
                    .checked_add(RETRY_INTERVAL)
                    .unwrap_or_else(|e| panic!("{e}"));
            } else {
                self.events.shutdown();
            }
            Ok(())
        }
    }
    // No host ever listens: each retry is a tick that changes nothing but the connection state. If the retry is
    // never drawn, the clock stops the run after 40 reads instead of letting it hang.
    let now = Arc::new(Mutex::new(
        "2026-10-29T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap_or_else(|e| panic!("{e}")),
    ));
    let stop: Arc<Mutex<Option<EventSender>>> = Arc::default();
    let clock = {
        let (now, stop) = (now.clone(), stop.clone());
        let mut reads = 0;
        move || {
            reads += 1;
            if reads > 40
                && let Some(events) = lock(&stop).as_ref()
            {
                events.shutdown();
            }
            Timestamp::parse(lock(&now).to_string()).unwrap_or_else(|e| panic!("{e}"))
        }
    };
    let net = FakeNetwork::new();
    let connect: Connector<FakeLink> =
        Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string()));
    let mut runtime = ClientRuntime::new(
        MachineId::new("m_client"),
        connect,
        FakeHostLauncher::new(),
        clock,
    );
    *lock(&stop) = Some(runtime.events());
    let mut view = Recorder {
        now,
        events: runtime.events(),
        attempts: Vec::new(),
    };
    assert_eq!(runtime.run_with(&mut view), Ok(()));
    assert_eq!(view.attempts, [1, 2], "the retry was drawn");
}

#[test]
fn a_flood_longer_than_a_batch_is_drawn_part_way() {
    /// Records the client's view size at its second draw, then quits.
    struct Recorder {
        events: EventSender,
        draws: usize,
        size_at_second: Option<PtySize>,
    }
    impl View for Recorder {
        fn size(&mut self) -> std::io::Result<(u16, u16)> {
            Ok(WINDOW)
        }
        fn draw(&mut self, client: &Client, _: &Screens) -> std::io::Result<()> {
            self.draws += 1;
            if self.draws == 2 {
                self.size_at_second = client.size();
                self.events.shutdown();
            }
            Ok(())
        }
    }
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    let events = h.runtime.events();
    let width = |k: usize| 100 + u16::try_from(k).unwrap_or_else(|e| panic!("{e}"));
    for k in 0..MAX_BATCH + 10 {
        events.terminal(Event::Resize(width(k), 30));
    }
    let mut view = Recorder {
        events,
        draws: 0,
        size_at_second: None,
    };
    assert_eq!(h.runtime.run_with(&mut view), Ok(()));
    // The second frame came after exactly one batch, not after the whole flood.
    assert_eq!(
        view.size_at_second,
        Some(pane_size(width(MAX_BATCH - 1), 30))
    );
}

// ---------- restoring the terminal ----------

/// What a terminal that supports the kitty keyboard protocol goes through, start to finish.
const RESTORED: [&str; 6] = [
    "enter",
    "push keyboard",
    "mouse on",
    "mouse off",
    "pop keyboard",
    "leave",
];

#[test]
fn quitting_restores_the_terminal() {
    let mut h = Harness::new();
    let terminal = FakeTerminal::enhanced();
    let mut view = test_view();
    let events = h.runtime.events();
    events.terminal(Event::Key(ctrl('b')));
    events.terminal(Event::Key(ch('q')));
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert_eq!(result.ok(), Some(Ok(())));
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn a_fatal_error_restores_the_terminal() {
    let mut h = Harness::with_launcher(|_, _| {
        FakeHostLauncher::new().failing(LaunchError::SpawnFailed("no such file".into()))
    });
    let terminal = FakeTerminal::enhanced();
    let mut view = test_view();
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert!(
        matches!(result, Ok(Err(ClientError::Fatal(_)))),
        "{result:?}"
    );
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn a_panic_while_drawing_restores_the_terminal() {
    struct Exploding;
    impl View for Exploding {
        fn size(&mut self) -> std::io::Result<(u16, u16)> {
            Ok(WINDOW)
        }
        fn draw(&mut self, _: &Client, _: &Screens) -> std::io::Result<()> {
            panic!("drawing failed");
        }
    }
    let mut h = Harness::new();
    let terminal = FakeTerminal::enhanced();
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_terminal(setup, |_| h.runtime.run_with(&mut Exploding))
    }));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn a_terminal_that_cannot_be_read_stops_the_client_with_an_error_and_restores_it() {
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    let terminal = FakeTerminal::enhanced();
    let mut view = test_view();
    h.runtime.events().terminal_failed("stdin closed".into());
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert_eq!(
        result.ok(),
        Some(Err(ClientError::Terminal("stdin closed".into())))
    );
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn sighup_sigterm_and_sigint_each_quit_the_client_so_the_terminal_is_restored() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    // One test for all three: a raised signal reaches every client forwarding signals in this process.
    for signal in [SIGHUP, SIGTERM, SIGINT] {
        // No host ever answers, so only the signal can end the run.
        let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
        let terminal = FakeTerminal::enhanced();
        let mut view = test_view();
        forward_signals(h.runtime.events()).unwrap_or_else(|e| panic!("{e}"));
        signal_hook::low_level::raise(signal).unwrap_or_else(|e| panic!("{e}"));
        let setup: Arc<dyn TerminalSetup> = terminal.clone();
        let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
        assert_eq!(result.ok(), Some(Ok(())), "signal {signal}");
        assert_eq!(terminal.calls(), RESTORED, "signal {signal}");
    }
}

#[test]
fn a_session_that_goes_idle_while_unselected_rings_the_terminal_and_is_marked_until_clicked() {
    let mut h = Harness::with_launcher(|net, host| {
        FakeHostLauncher::new().on_launch(move || {
            *lock(&host) = Some(FakeHost::start(&net).with_bell());
        })
    });
    let terminal = FakeTerminal::new();
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    with_terminal(setup, |restorer| {
        h.runtime.set_terminal(restorer.clone());
        h.runtime.start();
        h.settle();
        h.wait_retry();
        h.user(Intent::NewSession);
        h.user(Intent::NewSession);
        assert_eq!(h.selected().as_deref(), Some("s2"));
        lock(&h.host)
            .as_mut()
            .unwrap_or_else(|| panic!("no host"))
            .agent_idle("s1");
        h.settle();
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(terminal.alerts(), ["shepherd: shell-1 is idle"]);
    let s1 = |h: &Harness| h.runtime.client().sessions()[1].attention;
    assert!(s1(&h), "marked while unselected");
    h.user(Intent::SelectSession(SessionId::new("s1")));
    assert!(!s1(&h), "cleared once selected");
}

// ---------- workspaces ----------

#[test]
fn the_client_tells_the_host_the_directory_it_started_in() {
    let mut h = Harness::new();
    h.runtime.set_cwd("/src/app");
    h.runtime.start();
    h.settle();
    h.wait_retry();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    assert_eq!(
        h.host(|host| host.hello_cwds().to_vec()),
        [Some("/src/app".to_owned())]
    );
}

#[test]
fn clicking_the_workspace_and_opening_a_typed_path_switches_to_it() {
    let mut h = Harness::ready();
    let mut view = test_view();
    h.draw(&mut view);
    h.click(find(&view, "⌂ choose a workspace"));
    h.typed("/src/app");
    h.draw(&mut view);
    h.click(find(&view, "[Open]"));
    h.draw(&mut view);
    assert_eq!(h.runtime.client().popup(), None);
    find(&view, "⌂ app");

    // Back to the switcher: the workspace used is listed, and clicking it keeps it.
    h.click(find(&view, "⌂ app"));
    h.draw(&mut view);
    h.click(find(&view, "/src/app"));
    h.draw(&mut view);
    assert_eq!(h.runtime.client().popup(), None);
    assert_eq!(
        h.runtime.client().workspace(),
        Some(std::path::Path::new("/src/app"))
    );
}

// ---------- display polish ----------

/// A ready harness with session `s1`, which has begun a synchronized update that it never ends and printed `held`
/// inside it. Returns the hold's timeout.
fn open_hold() -> (Harness, jiff::SignedDuration) {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    // The fake host echoes input.
    h.user(Intent::keys(b"\x1b[?2026hheld".to_vec()));
    let hold = h
        .runtime
        .screen(&SessionId::new("s1"))
        .and_then(Screen::sync_hold);
    let hold = hold.unwrap_or_else(|| panic!("no hold"));
    assert_eq!(h.screen("s1"), "");
    let timeout = jiff::SignedDuration::try_from(hold.timeout).unwrap_or_else(|e| panic!("{e}"));
    (h, timeout)
}

#[test]
fn output_inside_an_open_hold_doesnt_put_off_its_deadline() {
    let (mut h, timeout) = open_hold();
    h.advance(timeout / 2);
    h.user(Intent::keys(b"more".to_vec()));
    assert_eq!(h.screen("s1"), "");
    h.advance(timeout - timeout / 2);
    assert!(h.runtime.tick(), "a released hold redraws");
    assert_eq!(h.screen("s1"), "heldmore");
}

#[test]
fn a_closed_sessions_hold_is_forgotten() {
    let (mut h, timeout) = open_hold();
    h.user(Intent::BeginClose);
    h.user(Intent::Confirm);
    assert!(h.runtime.screen(&SessionId::new("s1")).is_none());
    h.advance(timeout);
    assert!(!h.runtime.tick());
}

#[test]
fn a_session_stuck_in_a_synchronized_update_is_drawn_once_its_hold_times_out() {
    let (mut h, timeout) = open_hold();
    h.advance(timeout - jiff::SignedDuration::from_millis(1));
    assert!(!h.runtime.tick());
    assert_eq!(h.screen("s1"), "");
    h.advance(jiff::SignedDuration::from_millis(1));
    assert!(h.runtime.tick(), "a released hold redraws");
    assert_eq!(h.screen("s1"), "held");
}

#[test]
fn a_tick_that_clears_the_notice_redraws() {
    // Never started, so no tick changes the connection.
    let mut h = Harness::new();
    h.user(Intent::NewSession);
    assert!(h.runtime.client().notice().is_some());
    // The first tick starts the notice's time.
    assert!(!h.runtime.tick());
    h.advance(NOTICE_TIMEOUT);
    assert!(h.runtime.tick());
    assert_eq!(h.runtime.client().notice(), None);
}

#[test]
fn the_selected_sessions_cursor_shape_reaches_the_real_terminal() {
    const BAR: &[u8] = b"\x1b[5 q";
    const USERS_OWN: &[u8] = b"\x1b[0 q";
    let mut h = Harness::ready();
    let options = ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, WINDOW.0, WINDOW.1)),
    };
    /// What the real terminal is sent.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Written {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let out = Written::default();
    let terminal = ratatui::Terminal::with_options(CrosstermBackend::new(out.clone()), options)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut view = TerminalView::new(terminal);
    let written = |seq: &[u8]| {
        lock(&out.0)
            .windows(seq.len())
            .filter(|w| *w == seq)
            .count()
    };
    let draw = |h: &Harness, view: &mut TerminalView<CrosstermBackend<Written>>| {
        view.draw(h.runtime.client(), h.runtime.screens())
            .unwrap_or_else(|e| panic!("{e}"));
    };

    h.keys(&[ctrl('b'), ch('n')]);
    // The fake host echoes it: the session asks for a blinking bar.
    h.user(Intent::keys(BAR.to_vec()));
    draw(&h, &mut view);
    assert_eq!(written(BAR), 1);
    // Unchanged: not written again.
    draw(&h, &mut view);
    assert_eq!(written(BAR), 1);

    // Under a popup the cursor is the user's own again.
    h.user(Intent::ShowHelp);
    draw(&h, &mut view);
    assert_eq!(written(USERS_OWN), 1);
}

// ---------- a host of another build (T36a) ----------

fn build(byte: u8) -> BuildId {
    BuildId {
        version: "0.1.0".into(),
        digest: ExeDigest::from_bytes([byte; 32]),
    }
}

/// A client of build B finds a host still running build A, while B is what's installed: it offers to restart the host,
/// and only a click on `[Restart host]` does. The host stops, the reconnect launches the installed build in its place,
/// and that one matches.
#[test]
fn a_stale_host_is_restarted_when_the_user_clicks_restart_and_the_reconnect_relaunches_it() {
    let (a, b) = (build(0xa), build(0xb));
    let mut h = Harness::with_launcher(|net, host| {
        let stopped = host.clone();
        let mut launched = 0;
        let installed = b.digest;
        let (a, b) = (a.clone(), b.clone());
        FakeHostLauncher::new()
            .on_launch(move || {
                launched += 1;
                let running = if launched == 1 { a.clone() } else { b.clone() };
                *lock(&host) = Some(FakeHost::start(&net).with_build(running, installed));
            })
            // The host shuts down: its listener and every connection go.
            .on_terminate(move || drop(lock(&stopped).take()))
    });
    h.runtime.set_build(b.clone());
    h.runtime.start();
    h.settle();
    h.wait_retry();
    h.term(Event::Resize(WINDOW.0, WINDOW.1));
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    assert!(
        matches!(h.runtime.client().popup(), Some(Popup::RestartHost { .. })),
        "{:?}",
        h.runtime.client().popup()
    );
    assert_eq!(h.launcher.terminations(), 0, "never without a click");

    let mut view = test_view();
    h.draw(&mut view);
    h.click(find(&view, "[Restart host]"));
    assert_eq!(h.launcher.terminations(), 1);
    // The link dropped with the host: the client reconnects, finds nothing, and launches the installed build.
    h.wait_retry();
    assert_eq!(h.launcher.launches(), 2);
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    assert_eq!(h.runtime.client().popup(), None);
    assert_eq!(
        h.host(|host| host.hello_cwds().len()),
        1,
        "the new host's hello"
    );
}

/// The host was gone by the time the user clicked: nothing to stop is not a failure, so no "could not restart".
#[test]
fn restarting_a_host_that_is_already_gone_says_nothing_failed() {
    let (a, b) = (build(0xa), build(0xb));
    // A host started by someone else; the launcher knows of none running, so its terminate answers NotRunning.
    let mut h = Harness::with_launcher(|net, host| {
        *lock(&host) = Some(FakeHost::start(&net).with_build(a, b.digest));
        FakeHostLauncher::new()
    });
    h.runtime.set_build(b);
    h.runtime.start();
    h.settle();
    h.term(Event::Resize(WINDOW.0, WINDOW.1));
    assert!(matches!(
        h.runtime.client().popup(),
        Some(Popup::RestartHost { .. })
    ));
    let mut view = test_view();
    h.draw(&mut view);
    h.click(find(&view, "[Restart host]"));
    assert_eq!(h.launcher.terminations(), 0);
    let notice = h.runtime.client().notice().unwrap_or_default().to_owned();
    assert!(!notice.contains("could not restart"), "{notice}");
}

#[test]
fn not_now_leaves_the_stale_host_running() {
    let (a, b) = (build(0xa), build(0xb));
    let mut h = Harness::with_launcher(|net, host| {
        let installed = b.digest;
        FakeHostLauncher::new().on_launch(move || {
            *lock(&host) = Some(FakeHost::start(&net).with_build(a.clone(), installed));
        })
    });
    h.runtime.set_build(b);
    h.runtime.start();
    h.settle();
    h.wait_retry();
    h.term(Event::Resize(WINDOW.0, WINDOW.1));
    let mut view = test_view();
    h.draw(&mut view);
    h.click(find(&view, "[Not now]"));
    assert_eq!(h.runtime.client().popup(), None);
    assert_eq!((h.launcher.terminations(), h.launcher.launches()), (0, 1));
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
}

/// A client of build B finds a host still running build A, with B installed: it asks the host to update itself in
/// place (T36b), says so, and once the host has re-execed (here: a host of build B takes its place on the same
/// socket, no launch) the reconnect finds B and says "host updated". Nothing is restarted and no dialog shows.
#[test]
fn a_stale_host_is_asked_to_update_itself_and_the_reconnect_says_it_did() {
    let (a, b) = (build(0xa), build(0xb));
    let mut h = Harness::with_launcher(|net, host| {
        let installed = b.digest;
        let a = a.clone();
        FakeHostLauncher::new().on_launch(move || {
            *lock(&host) = Some(
                FakeHost::start(&net)
                    .with_build(a.clone(), installed)
                    .with_upgrade_answers([UpgradeAnswer::Accepted]),
            );
        })
    });
    h.runtime.set_build(b.clone());
    h.runtime.start();
    h.settle();
    h.wait_retry();
    assert_eq!(h.host(FakeHost::upgrades), 1);
    assert_eq!(h.runtime.client().popup(), None);
    assert_eq!(
        h.runtime.client().notice(),
        Some("host is older — updating…")
    );

    // The re-exec: the old image's connections close, and the new one listens on the same socket.
    drop(lock(&h.host).take());
    *lock(&h.host) = Some(FakeHost::start(&h.net).with_build(b.clone(), b.digest));
    h.settle();
    h.wait_retry();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    assert_eq!(h.runtime.client().notice(), Some("host updated"));
    assert_eq!(h.runtime.client().popup(), None);
    assert_eq!((h.launcher.launches(), h.launcher.terminations()), (1, 0));
}
