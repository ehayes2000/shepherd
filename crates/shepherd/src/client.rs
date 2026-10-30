//! The client runtime: runs [`Client`] against a [`Link`] and a [`HostLauncher`], keeps a terminal [`Screen`] per
//! session fed with its PTY output, and draws the TUI.
//!
//! Everything arrives on one unbounded channel: link events (from the link's own threads, through a sink that never
//! blocks), user intents, terminal events (keys, mouse, paste, focus, resize; mapped by [`shepherd_tui::map_event`]),
//! and shutdown requests from signals. [`ClientRuntime::pump`] handles what is queued without blocking, which is how
//! scenario tests drive it step by step. [`ClientRuntime::run_with`] is the real loop: it blocks on the channel, ticks
//! the client every [`TICK`] with the clock's time so connect retries fire, and redraws a [`View`] once per batch of
//! inputs, so a flood of output isn't drawn message by message.
//!
//! [`run`] is `shepherd` itself: the real terminal (see [`crate::terminal`]) and the local host's socket.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::Event;
use ratatui::layout::Rect;
use shepherd_tui::{Pointer, Screens, from_legacy_terminal, map_event};

use shepherd_adapters::{ProcessHostLauncher, UnixConnectLink};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{Client, ClientCommand, ClientEvent, Intent};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{ConnId, HostLauncher, Link, LinkEvent, LinkEventSink};
use shepherd_core::wire::{
    ClientMessage, HostMessage, HostNotification, decode_host_message, encode_client_notification,
    encode_request,
};
use shepherd_term::Screen;

use crate::screen_guard;
use crate::terminal::{CrosstermSetup, Restorer, TerminalSetup, install_panic_hook, with_terminal};

/// How often [`ClientRuntime::run`] ticks the client.
pub const TICK: Duration = Duration::from_millis(50);
/// The most queued inputs [`ClientRuntime::run_with`] handles before it draws, so a flood still shows progress.
pub const MAX_BATCH: usize = 1024;
/// The screen size before the TUI reports the view's.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Opens a client link to the host, sending its events to the sink. The real one connects to the unix socket.
pub type Connector<L> =
    Box<dyn FnMut(Arc<dyn LinkEventSink>) -> Result<(L, ConnId), String> + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The client gave up. The message is for the user.
    Fatal(String),
    /// The terminal couldn't be set up, read or drawn on.
    Terminal(String),
    /// A thread panicked. Its message was printed when it happened.
    Panicked,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fatal(why) => f.write_str(why),
            Self::Terminal(why) => write!(f, "terminal: {why}"),
            Self::Panicked => f.write_str("a thread panicked"),
        }
    }
}

impl std::error::Error for ClientError {}

/// What the runtime's channel carries.
enum Input {
    /// An event from the link opened by connect number `generation`.
    Link {
        generation: u64,
        event: LinkEvent,
    },
    User(Intent),
    /// Something the user did in the terminal.
    Terminal(Event),
    /// The terminal can't be read any more.
    TerminalFailed(String),
    /// A thread panicked.
    Panicked,
    /// Quit, as if the user detached (a signal).
    Shutdown,
}

/// Forwards one link's events, tagged with its generation, so events from an earlier link are told apart.
struct Sink {
    generation: u64,
    tx: Sender<Input>,
}

impl LinkEventSink for Sink {
    fn send(&self, event: LinkEvent) {
        let generation = self.generation;
        let _ = self.tx.send(Input::Link { generation, event });
    }
}

/// Pushes user intents into a running client, from any thread.
#[derive(Clone)]
pub struct IntentSender(Sender<Input>);

impl IntentSender {
    /// Returns false once the runtime is gone.
    pub fn send(&self, intent: Intent) -> bool {
        self.0.send(Input::User(intent)).is_ok()
    }
}

/// Pushes terminal events and shutdown requests into a running client, from any thread.
#[derive(Clone)]
pub struct EventSender(Sender<Input>);

impl EventSender {
    /// Returns false once the runtime is gone.
    pub fn terminal(&self, event: Event) -> bool {
        self.0.send(Input::Terminal(event)).is_ok()
    }

    /// The terminal can't be read: the client stops with [`ClientError::Terminal`].
    pub fn terminal_failed(&self, why: String) -> bool {
        self.0.send(Input::TerminalFailed(why)).is_ok()
    }

    /// Asks the client to quit. Sessions keep running on the host.
    pub fn shutdown(&self) -> bool {
        self.0.send(Input::Shutdown).is_ok()
    }

    /// A thread panicked: the client quits with [`ClientError::Panicked`]. Sessions keep running on the host.
    pub fn panicked(&self) -> bool {
        self.0.send(Input::Panicked).is_ok()
    }
}

/// Where the client is drawn.
pub trait View {
    /// The window's `(cols, rows)`.
    fn size(&mut self) -> io::Result<(u16, u16)>;
    fn draw(&mut self, client: &Client, screens: &Screens) -> io::Result<()>;
}

/// Draws with ratatui on `B`. The mouse is captured for the client's whole life by the terminal guard.
pub struct TerminalView<B: Backend> {
    terminal: ratatui::Terminal<B>,
}

impl<B: Backend> TerminalView<B> {
    pub fn new(terminal: ratatui::Terminal<B>) -> Self {
        Self { terminal }
    }

    pub fn terminal(&self) -> &ratatui::Terminal<B> {
        &self.terminal
    }
}

impl<B: Backend> View for TerminalView<B> {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        let size = self
            .terminal
            .size()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok((size.width, size.height))
    }

    fn draw(&mut self, client: &Client, screens: &Screens) -> io::Result<()> {
        self.terminal
            .draw(|frame| shepherd_tui::render(client, screens, frame))
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(())
    }
}

fn selected_screen<'a>(client: &Client, screens: &'a Screens) -> Option<&'a Screen> {
    screens.get(client.selected()?)
}

fn terminal_error(e: impl fmt::Display) -> ClientError {
    ClientError::Terminal(e.to_string())
}

pub struct ClientRuntime<L, C, H> {
    client: Client,
    connect: C,
    launcher: H,
    clock: Box<dyn FnMut() -> Timestamp + Send>,
    link: Option<(L, ConnId)>,
    generation: u64,
    tx: Sender<Input>,
    rx: Receiver<Input>,
    screens: Screens,
    /// The window's `(cols, rows)`, from the last resize.
    window: (u16, u16),
    /// Why the terminal stopped working, if it did.
    terminal_failed: Option<String>,
    /// A thread panicked.
    panicked: bool,
    /// The outer terminal reports keys unambiguously (kitty keyboard protocol).
    keyboard_enhanced: bool,
    /// Where a mouse press went down, so a release on the same control clicks it.
    pointer: Pointer,
    /// The terminal alerts go to, once the TUI has taken it over.
    terminal: Option<Restorer>,
    done: bool,
}

impl<L, C, H> ClientRuntime<L, C, H>
where
    L: Link,
    C: FnMut(Arc<dyn LinkEventSink>) -> Result<(L, ConnId), String>,
    H: HostLauncher,
{
    /// A runtime for a client on `machine`. `clock` gives the current time for retries.
    pub fn new(
        machine: MachineId,
        connect: C,
        launcher: H,
        clock: impl FnMut() -> Timestamp + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            client: Client::new(machine),
            connect,
            launcher,
            clock: Box::new(clock),
            link: None,
            generation: 0,
            tx,
            rx,
            screens: Screens::new(),
            window: DEFAULT_SIZE,
            terminal_failed: None,
            panicked: false,
            keyboard_enhanced: false,
            pointer: Pointer::default(),
            terminal: None,
            done: false,
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The terminal of `session`, once it has printed anything.
    pub fn screen(&self, session: &SessionId) -> Option<&Screen> {
        self.screens.get(session)
    }

    /// Every session's terminal.
    pub fn screens(&self) -> &Screens {
        &self.screens
    }

    pub fn intents(&self) -> IntentSender {
        IntentSender(self.tx.clone())
    }

    pub fn events(&self) -> EventSender {
        EventSender(self.tx.clone())
    }

    /// The client has quit, or given up.
    pub fn done(&self) -> bool {
        self.done
    }

    pub fn start(&mut self) {
        self.handle(ClientEvent::Start);
    }

    pub fn tick(&mut self) {
        let at = (self.clock)();
        self.handle(ClientEvent::Tick { at });
    }

    pub fn user(&mut self, intent: Intent) {
        if let Intent::Resize(size) = &intent {
            for screen in self.screens.values_mut() {
                screen.resize(size.cols, size.rows);
            }
        }
        self.handle(ClientEvent::User(intent));
    }

    /// Handles everything queued so far, without blocking. Returns how many inputs there were.
    pub fn pump(&mut self) -> usize {
        self.drain(usize::MAX)
    }

    /// Handles up to `max` queued inputs, without blocking. Returns how many there were.
    fn drain(&mut self, max: usize) -> usize {
        let mut handled = 0;
        while !self.done && handled < max {
            let Ok(input) = self.rx.try_recv() else {
                break;
            };
            self.input(input);
            handled += 1;
        }
        handled
    }

    /// Handles everything queued so far, without blocking, and draws `view` if there was anything. Returns whether
    /// it drew.
    pub fn pump_frame(&mut self, view: &mut dyn View) -> io::Result<bool> {
        let handled = self.pump();
        if handled > 0 {
            view.draw(&self.client, &self.screens)?;
        }
        Ok(handled > 0)
    }

    /// Sends alerts ([`ClientCommand::Alert`]) to the terminal `restorer` holds, while it's taken over. Without one
    /// they are dropped.
    pub fn set_terminal(&mut self, restorer: Restorer) {
        self.terminal = Some(restorer);
    }

    /// Whether the outer terminal reports keys unambiguously. Until it does, keys are read as a legacy terminal
    /// sends them (see [`from_legacy_terminal`]).
    pub fn set_keyboard_enhanced(&mut self, on: bool) {
        self.keyboard_enhanced = on;
    }

    /// A terminal event: what it means is decided by the TUI from the client's state and the selected session's
    /// input modes.
    pub fn terminal_event(&mut self, event: Event) {
        let event = match event {
            Event::Key(key) if !self.keyboard_enhanced => Event::Key(from_legacy_terminal(key)),
            Event::Resize(cols, rows) => {
                self.window = (cols, rows);
                event
            }
            event => event,
        };
        let modes = selected_screen(&self.client, &self.screens)
            .map(Screen::modes)
            .unwrap_or_default();
        let window = Rect::new(0, 0, self.window.0, self.window.1);
        for intent in map_event(&event, &self.client, &modes, window, &mut self.pointer) {
            self.user(intent);
        }
    }

    /// Starts the client and runs it, with nothing drawn, until it quits. Fails with the client's message if it
    /// gives up.
    pub fn run(&mut self) -> Result<(), ClientError> {
        self.run_loop(None)
    }

    /// Starts the client and runs it until it quits, drawing it on `view`.
    pub fn run_with(&mut self, view: &mut dyn View) -> Result<(), ClientError> {
        self.run_loop(Some(view))
    }

    fn run_loop(&mut self, mut view: Option<&mut dyn View>) -> Result<(), ClientError> {
        self.start();
        if let Some(view) = view.as_deref_mut() {
            let (cols, rows) = view.size().map_err(terminal_error)?;
            self.terminal_event(Event::Resize(cols, rows));
            if !self.done {
                view.draw(&self.client, &self.screens)
                    .map_err(terminal_error)?;
            }
        }
        let mut last_tick = Instant::now();
        while !self.done {
            let mut changed = false;
            match self
                .rx
                .recv_timeout(TICK.saturating_sub(last_tick.elapsed()))
            {
                Ok(input) => {
                    self.input(input);
                    changed = true;
                    // Everything else already queued is drawn with it.
                    self.drain(MAX_BATCH - 1);
                }
                Err(RecvTimeoutError::Timeout) => {}
                // The runtime holds a sender itself.
                Err(RecvTimeoutError::Disconnected) => unreachable!("the runtime's own sender"),
            }
            if !self.done && last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                let before = self.client.connection().clone();
                self.tick();
                changed |= *self.client.connection() != before;
            }
            if let Some(view) = view.as_deref_mut()
                && changed
                && !self.done
            {
                view.draw(&self.client, &self.screens)
                    .map_err(terminal_error)?;
            }
        }
        if self.panicked {
            return Err(ClientError::Panicked);
        }
        if let Some(why) = &self.terminal_failed {
            return Err(ClientError::Terminal(why.clone()));
        }
        match self.client.fatal() {
            Some(why) => Err(ClientError::Fatal(why.to_owned())),
            None => Ok(()),
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::User(intent) => self.user(intent),
            Input::Terminal(event) => self.terminal_event(event),
            Input::TerminalFailed(why) => {
                self.terminal_failed = Some(why);
                self.user(Intent::Quit);
            }
            Input::Panicked => {
                self.panicked = true;
                self.user(Intent::Quit);
            }
            Input::Shutdown => self.user(Intent::Quit),
            Input::Link { generation, .. } if generation != self.generation => {}
            Input::Link { event, .. } => match event {
                LinkEvent::Line { line, .. } => {
                    // A line this version can't read (e.g. an unknown notification) is skipped.
                    if let Ok(message) = decode_host_message(&line) {
                        self.host_message(message);
                    }
                }
                LinkEvent::Disconnected { .. } => {
                    self.link = None;
                    self.handle(ClientEvent::Disconnected);
                }
                LinkEvent::Connected { .. } => {}
            },
        }
    }

    fn host_message(&mut self, message: HostMessage) {
        if let HostMessage::Notification(HostNotification::PtyOutput(out)) = &message {
            let (cols, rows) = self
                .client
                .size()
                .map_or(DEFAULT_SIZE, |s| (s.cols, s.rows));
            let screen = self
                .screens
                .entry(out.session.clone())
                .or_insert_with(|| Screen::new(cols, rows));
            if let Err(panic) = screen_guard::update(screen, |s| s.feed(&out.bytes)) {
                // The client has no log file unless a subscriber is set; the host logs its own copy's panic.
                tracing::error!(session = %out.session, %panic, "screen panicked; replaced it with a blank one");
            }
            // The host's own screen answers the program's queries (decision 0007).
            screen.take_replies();
        }
        let log_entry = matches!(
            message,
            HostMessage::Notification(HostNotification::LogEntry(_))
        );
        self.handle(ClientEvent::Host(message));
        if log_entry {
            let sessions = self.client.sessions();
            self.screens
                .retain(|id, _| sessions.iter().any(|s| &s.id == id));
        }
    }

    /// Feeds `event` to the client and executes its commands, along with the events those produce.
    fn handle(&mut self, event: ClientEvent) {
        let mut queue = VecDeque::from([event]);
        while let Some(event) = queue.pop_front() {
            if self.done {
                return;
            }
            for command in self.client.handle(event) {
                queue.extend(self.execute(command));
            }
        }
    }

    fn execute(&mut self, command: ClientCommand) -> Option<ClientEvent> {
        match command {
            ClientCommand::Send(message) => {
                let line = match &message {
                    ClientMessage::Request(r) => encode_request(r),
                    ClientMessage::Notification(n) => encode_client_notification(n),
                };
                let line = line.strip_suffix(b"\n").unwrap_or(&line);
                let (link, conn) = self.link.as_mut()?;
                if link.send(*conn, line).is_err() && link.close(*conn).is_err() {
                    // The link already forgot the conn; its `Disconnected` won't come.
                    self.link = None;
                    return Some(ClientEvent::Disconnected);
                }
                None
            }
            ClientCommand::LaunchHost => {
                self.launcher
                    .launch()
                    .err()
                    .map(|e| ClientEvent::LaunchFailed {
                        error: e.to_string(),
                    })
            }
            ClientCommand::Connect => {
                self.generation += 1;
                let sink = Arc::new(Sink {
                    generation: self.generation,
                    tx: self.tx.clone(),
                });
                match (self.connect)(sink) {
                    Ok(link) => {
                        self.link = Some(link);
                        Some(ClientEvent::Connected { at: (self.clock)() })
                    }
                    Err(error) => Some(ClientEvent::ConnectFailed {
                        at: (self.clock)(),
                        error,
                    }),
                }
            }
            ClientCommand::Alert { message, .. } => {
                if let Some(terminal) = &self.terminal {
                    terminal.alert(&message);
                }
                None
            }
            ClientCommand::Quit => {
                self.done = true;
                if let Some((mut link, conn)) = self.link.take() {
                    let _ = link.close(conn);
                }
                None
            }
        }
    }
}

/// Runs `shepherd`: the TUI in the real terminal, against the local host's socket at `socket`, launching
/// `shepherd host` if nothing listens. The terminal is restored however this ends.
pub fn run(socket: &Path) -> Result<(), ClientError> {
    let launcher = ProcessHostLauncher::shepherd_host()
        .map_err(|e| ClientError::Fatal(format!("can't find the shepherd executable: {e}")))?;
    let socket = socket.to_path_buf();
    let connect: Connector<UnixConnectLink> =
        Box::new(move |sink| UnixConnectLink::connect(&socket, sink).map_err(|e| e.to_string()));
    let machine = MachineId::new("local");
    let start = Instant::now();
    let clock = monotonic(jiff::Timestamp::now(), move || start.elapsed());
    let mut runtime = ClientRuntime::new(machine, connect, launcher, clock);
    let events = runtime.events();
    forward_signals(events.clone()).map_err(terminal_error)?;
    let backend =
        ratatui::Terminal::new(CrosstermBackend::new(io::stdout())).map_err(terminal_error)?;
    let setup: Arc<dyn TerminalSetup> = Arc::new(CrosstermSetup);
    with_terminal(setup.clone(), |restorer| {
        install_client_panic_hook(restorer.clone(), events.clone());
        runtime.set_keyboard_enhanced(restorer.keyboard_enhanced());
        runtime.set_terminal(restorer.clone());
        read_terminal_events(events)?;
        runtime.run_with(&mut TerminalView::new(backend))
    })
    .map_err(terminal_error)?
}

/// On a panic on any thread: restores the terminal, prints the message, and stops the client with
/// [`ClientError::Panicked`], so `shepherd` exits non-zero.
pub fn install_client_panic_hook(restorer: Restorer, events: EventSender) {
    install_panic_hook(restorer, move || {
        events.panicked();
    });
}

/// Turns SIGTERM, SIGHUP and SIGINT into [`EventSender::shutdown`], so the client quits through its normal path and
/// the terminal is restored.
pub fn forward_signals(events: EventSender) -> io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP, SIGINT])?;
    std::thread::Builder::new()
        .name("client-signals".into())
        .spawn(move || {
            for _ in signals.forever() {
                if !events.shutdown() {
                    return;
                }
            }
        })?;
    Ok(())
}

/// Reads the real terminal's events on a thread, into the runtime's channel.
fn read_terminal_events(events: EventSender) -> Result<(), ClientError> {
    std::thread::Builder::new()
        .name("client-terminal".into())
        .spawn(move || {
            loop {
                match ratatui::crossterm::event::read() {
                    Ok(event) => {
                        if !events.terminal(event) {
                            return;
                        }
                    }
                    Err(e) => {
                        events.terminal_failed(e.to_string());
                        return;
                    }
                }
            }
        })
        .map(drop)
        .map_err(terminal_error)
}

/// A clock for core that never goes backwards: the wall time `anchor` read once, plus the monotonic time
/// `elapsed` since then. The retry and hello deadlines core computes from it survive the wall clock being stepped.
fn monotonic(
    anchor: jiff::Timestamp,
    mut elapsed: impl FnMut() -> Duration + Send + 'static,
) -> impl FnMut() -> Timestamp + Send + 'static {
    move || {
        let since = jiff::SignedDuration::try_from(elapsed()).unwrap_or(jiff::SignedDuration::MAX);
        let now = anchor.checked_add(since).unwrap_or(jiff::Timestamp::MAX);
        Timestamp::parse(now.to_string())
            .unwrap_or_else(|e| unreachable!("jiff formats RFC 3339: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn the_monotonic_clock_is_its_wall_anchor_plus_monotonic_elapsed_time() {
        let anchor: jiff::Timestamp = "2026-10-29T12:00:00Z"
            .parse()
            .unwrap_or_else(|e| panic!("{e}"));
        let elapsed = Arc::new(Mutex::new(Duration::ZERO));
        let source = elapsed.clone();
        let mut clock = monotonic(anchor, move || {
            *source.lock().unwrap_or_else(|e| e.into_inner())
        });
        let mut seen = Vec::new();
        for ms in [0, 40, 40, 1500] {
            *elapsed.lock().unwrap_or_else(|e| e.into_inner()) = Duration::from_millis(ms);
            seen.push(clock().instant());
        }
        let expected: Vec<_> = [0, 40, 40, 1500]
            .into_iter()
            .map(|ms| anchor + jiff::SignedDuration::from_millis(ms))
            .collect();
        assert_eq!(seen, expected);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
    }
}
