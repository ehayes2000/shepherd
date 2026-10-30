//! The client runtime: runs [`Client`] against a [`Link`] and a [`HostLauncher`], and keeps a terminal
//! [`Screen`] per session fed with its PTY output. Rendering and keys are the TUI's (T13), which pushes intents
//! through [`ClientRuntime::intents`].
//!
//! Everything arrives on one unbounded channel: link events (from the link's own threads, through a sink that never
//! blocks) and user intents. [`ClientRuntime::pump`] handles what is queued without blocking, which is how scenario
//! tests drive it step by step. [`ClientRuntime::run`] is the real loop: it blocks on the channel and ticks the client
//! every [`TICK`] with the clock's time, so connect retries fire.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

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

/// How often [`ClientRuntime::run`] ticks the client.
pub const TICK: Duration = Duration::from_millis(50);
/// The screen size before the TUI reports the view's.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Opens a client link to the host, sending its events to the sink. The real one connects to the unix socket.
pub type Connector<L> =
    Box<dyn FnMut(Arc<dyn LinkEventSink>) -> Result<(L, ConnId), String> + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The client gave up. The message is for the user.
    Fatal(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fatal(why) => f.write_str(why),
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

pub struct ClientRuntime<L, C, H> {
    client: Client,
    connect: C,
    launcher: H,
    clock: Box<dyn FnMut() -> Timestamp + Send>,
    link: Option<(L, ConnId)>,
    generation: u64,
    tx: Sender<Input>,
    rx: Receiver<Input>,
    screens: BTreeMap<SessionId, Screen>,
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
            screens: BTreeMap::new(),
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

    pub fn intents(&self) -> IntentSender {
        IntentSender(self.tx.clone())
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
        let mut handled = 0;
        while !self.done {
            let Ok(input) = self.rx.try_recv() else {
                break;
            };
            self.input(input);
            handled += 1;
        }
        handled
    }

    /// Starts the client and runs it until it quits. Fails with the client's message if it gives up.
    pub fn run(&mut self) -> Result<(), ClientError> {
        self.start();
        let mut last_tick = Instant::now();
        while !self.done {
            match self.rx.recv_timeout(TICK) {
                Ok(input) => self.input(input),
                Err(RecvTimeoutError::Timeout) => {}
                // The runtime holds a sender itself.
                Err(RecvTimeoutError::Disconnected) => unreachable!("the runtime's own sender"),
            }
            if !self.done && last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.tick();
            }
        }
        match self.client.fatal() {
            Some(why) => Err(ClientError::Fatal(why.to_owned())),
            None => Ok(()),
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::User(intent) => self.user(intent),
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
            screen.feed(&out.bytes);
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

/// Runs the client against the local host's socket at `socket`, launching `shepherd host` if nothing listens.
pub fn run(socket: &Path) -> Result<(), ClientError> {
    let launcher = ProcessHostLauncher::shepherd_host()
        .map_err(|e| ClientError::Fatal(format!("can't find the shepherd executable: {e}")))?;
    let socket = socket.to_path_buf();
    let connect: Connector<UnixConnectLink> =
        Box::new(move |sink| UnixConnectLink::connect(&socket, sink).map_err(|e| e.to_string()));
    let machine = MachineId::new("local");
    let start = Instant::now();
    let clock = monotonic(jiff::Timestamp::now(), move || start.elapsed());
    ClientRuntime::new(machine, connect, launcher, clock).run()
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
