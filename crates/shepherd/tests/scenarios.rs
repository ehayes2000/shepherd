//! Scenario tests: user flows across a host and client runtime, wired with testkit fakes.
//! One scenario per user-visible feature. Edge cases belong in core tests.

mod support {
    pub mod fake_host;
    pub mod fake_terminal;
}

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use shepherd::client::{
    ClientError, ClientRuntime, Connector, EventSender, TerminalView, View, forward_signals,
};
use shepherd::terminal::{TerminalSetup, with_terminal};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{
    Client, Connection, HELLO_TIMEOUT, Intent, MAX_CONNECT_ATTEMPTS, RETRY_INTERVAL,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::LaunchError;
use shepherd_term::Screen;
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

    h.user(Intent::Input(b"two".to_vec()));
    h.user(Intent::NextSession);
    assert_eq!(h.selected().as_deref(), Some("s1"));
    h.user(Intent::Input(b"one".to_vec()));

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
    h.user(Intent::Input(b"hi".to_vec()));
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

fn test_view(terminal: &Arc<FakeTerminal>) -> TerminalView<TestBackend> {
    let backend =
        ratatui::Terminal::new(TestBackend::new(WINDOW.0, WINDOW.1)).unwrap_or_else(|e| match e {});
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    TerminalView::new(backend, setup)
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
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    let pane = layout(Rect::new(0, 0, WINDOW.0, WINDOW.1)).pane;
    let side = 0..pane.x - 1;
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
    // Newest first: s2 (`shell-2`) on top, s1 below it and selected.
    let buf = view.terminal().backend().buffer().clone();
    assert!(text(&view, side.clone(), 1).ends_with("shell-2"), "{buf:?}");
    assert!(text(&view, side.clone(), 3).ends_with("shell-1"), "{buf:?}");
    let highlighted = |y: u16| buf[(2, y)].bg != ratatui::style::Color::Reset;
    assert_eq!((highlighted(1), highlighted(3)), (false, true));

    h.keys(&[ctrl('b'), ch('k')]);
    h.draw(&mut view);
    assert_eq!(text(&view, screen, 0), "hello");
}

#[test]
fn the_selected_session_is_resized_to_the_pane_not_the_window() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    h.term(Event::Resize(120, 40));
    let pane = pane_size(120, 40);
    let last = h.host(|host| host.resizes().last().cloned());
    let last = last.unwrap_or_else(|| panic!("no resize sent"));
    assert_eq!(
        (last.session.as_str(), last.cols, last.rows),
        ("s1", pane.cols, pane.rows)
    );
    // The client's own screen of the session matches too.
    h.typed("x");
    let screen = h.runtime.screen(&SessionId::new("s1")).map(Screen::size);
    assert_eq!(screen, Some((pane.cols, pane.rows)));
}

#[test]
fn a_flood_of_output_is_drawn_once() {
    let mut h = Harness::ready();
    h.keys(&[ctrl('b'), ch('n')]);
    // 50 echoes queue up in the client before it gets to run.
    for _ in 0..50 {
        h.runtime.user(Intent::Input(b"x".to_vec()));
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
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    h.keys(&[ctrl('b'), ch('n')]);
    h.draw(&mut view);
    assert!(terminal.calls().is_empty(), "no capture until asked");

    // The fake host echoes input, so this turns on SGR mouse reporting in the session's terminal.
    h.user(Intent::Input(b"\x1b[?1000h\x1b[?1006h".to_vec()));
    h.draw(&mut view);
    assert_eq!(terminal.calls(), ["mouse on"]);

    let pane = layout(Rect::new(0, 0, WINDOW.0, WINDOW.1)).pane;
    h.term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: pane.x + 2,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    let last = h.inputs().last().cloned();
    assert_eq!(last, Some(("s1".into(), "\x1b[<64;3;2M".into())));

    // Selecting a session without mouse mode gives the mouse back to the outer terminal.
    h.keys(&[ctrl('b'), ch('n')]);
    h.draw(&mut view);
    assert_eq!(terminal.calls(), ["mouse on", "mouse off"]);
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

// ---------- restoring the terminal ----------

#[test]
fn quitting_restores_the_terminal() {
    let mut h = Harness::new();
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    let events = h.runtime.events();
    events.terminal(Event::Key(ctrl('b')));
    events.terminal(Event::Key(ch('q')));
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert_eq!(result.ok(), Some(Ok(())));
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn a_fatal_error_restores_the_terminal() {
    let mut h = Harness::with_launcher(|_, _| {
        FakeHostLauncher::new().failing(LaunchError::SpawnFailed("no such file".into()))
    });
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert!(
        matches!(result, Ok(Err(ClientError::Fatal(_)))),
        "{result:?}"
    );
    assert_eq!(terminal.calls(), ["enter", "leave"]);
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
    let terminal = FakeTerminal::new();
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_terminal(setup, |_| h.runtime.run_with(&mut Exploding))
    }));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn a_terminal_that_cannot_be_read_stops_the_client_with_an_error_and_restores_it() {
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    h.runtime.events().terminal_failed("stdin closed".into());
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert_eq!(
        result.ok(),
        Some(Err(ClientError::Terminal("stdin closed".into())))
    );
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn sighup_quits_the_client_so_the_terminal_is_restored() {
    // No host ever answers, so only the signal can end the run.
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    let terminal = FakeTerminal::new();
    let mut view = test_view(&terminal);
    forward_signals(h.runtime.events()).unwrap_or_else(|e| panic!("{e}"));
    signal_hook::low_level::raise(signal_hook::consts::SIGHUP).unwrap_or_else(|e| panic!("{e}"));
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = with_terminal(setup, |_| h.runtime.run_with(&mut view));
    assert_eq!(result.ok(), Some(Ok(())));
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}
