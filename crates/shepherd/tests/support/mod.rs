//! The scenario harness: a host runtime on all-fake ports, fake clients on a [`FakeNetwork`], and a loop stepper.
//! Shared by every scenario file (`mod support;`), so not every helper is used by each.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

/// A scripted host for client scenarios (T12).
pub mod fake_host;
/// A scripted terminal for TUI scenarios (T13).
pub mod fake_terminal;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use shepherd::host::{Env, HostRuntime, RuntimeEvent, RuntimeSettings, runtime_channel};
use shepherd_core::acpp::{EntryId, Envelope, RequestId, Seq, Timestamp};
use shepherd_core::config::{Config, RawConfig, RawDefaults};
use shepherd_core::host::Host;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{ConnId, EventLog, Link, LinkEvent, LinkEventSink, PtyEventSink};
use shepherd_core::wire::{
    ClientNotification, ClientRequest, HostMessage, HostNotification, InputKind, Request,
    SessionInput, SessionNew, SessionResize, Subscribe, SubscribeFrom, decode_host_message,
    encode_client_notification, encode_request,
};
use shepherd_testkit::fakes::commands::FakeCommands;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use shepherd_testkit::fakes::pty::{FakePty, ProgramInput, ScriptedProcess};
use shepherd_testkit::fakes::worktrees::FakeWorktrees;

/// The login shell the harness's config resolves the `shell` profile to.
pub const SHELL: &str = "/bin/fake-sh";
/// The home directory new sessions start in. It must exist, since the host refuses to spawn in a missing directory;
/// the fake PTY never writes into it, so any existing directory does: this crate's.
pub const HOME: &str = env!("CARGO_MANIFEST_DIR");
/// The `shepherd` executable the host writes into agents' hook settings.
pub const SHEPHERD_EXE: &str = "/opt/shepherd/bin/shepherd";
/// The host socket the host tells agents about.
pub const SOCKET: &str = "/home/test/.shepherd/host.sock";
/// A run root for runtimes that never launch an agent with run files: nothing can create it.
const NO_RUN_ROOT: &str = "/nonexistent/shepherd-run";
/// The managed worktree root of the harness's fake `Worktrees`, which knows no repos unless a test adds them.
pub const WORKTREE_ROOT: &str = "/home/test/.shepherd/worktrees";
/// How long [`HostHarness::step`] waits for a port call on a worker thread: a liveness bound, never a measurement.
const WORKER_LIMIT: Duration = Duration::from_secs(10);

/// The harness's config: the defaults, except that new sessions without a profile run the `shell` profile, so
/// scenarios don't need a fake `claude`.
pub fn shell_default_config() -> RawConfig {
    RawConfig {
        defaults: Some(RawDefaults {
            agent: Some("shell".to_owned()),
            env: None,
        }),
        ..RawConfig::default()
    }
}

/// A deterministic [`Env`]: session ids `s_t1`, `s_t2`, ..., entry ids `e1`, `e2`, ..., timestamps 1 ms apart from
/// `2026-10-29T12:00:00.000Z`, and a monotonic clock that only moves through [`Clock::advance`]. Advancing the clock
/// moves the timestamps by as much, so wall and monotonic time agree.
#[derive(Clone)]
pub struct TestEnv {
    counters: Arc<Mutex<Counters>>,
    clock: Clock,
}

#[derive(Default)]
struct Counters {
    sessions: u64,
    entries: u64,
    ticks: u64,
}

/// The test's handle on a [`TestEnv`]'s monotonic clock.
#[derive(Clone)]
pub struct Clock {
    base: Instant,
    offset: Arc<Mutex<Duration>>,
}

impl Clock {
    pub fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    pub fn now(&self) -> Instant {
        self.base + self.elapsed()
    }

    /// How far the clock has been advanced.
    pub fn elapsed(&self) -> Duration {
        *self.offset.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TestEnv {
    pub fn new() -> Self {
        Self {
            counters: Arc::default(),
            clock: Clock {
                // Only a base: nothing asserts on real time, and the clock moves only through `advance`.
                base: Instant::now(),
                offset: Arc::default(),
            },
        }
    }

    pub fn clock(&self) -> Clock {
        self.clock.clone()
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Env for TestEnv {
    fn now(&mut self) -> Timestamp {
        let ms = {
            let mut c = self.counters();
            c.ticks += 1;
            c.ticks
        };
        let ms = ms + u64::try_from(self.clock.elapsed().as_millis()).expect("test clock");
        let (secs, ms) = (ms / 1000, ms % 1000);
        let (mins, secs) = (secs / 60, secs % 60);
        Timestamp::parse(format!("2026-10-29T12:{mins:02}:{secs:02}.{ms:03}Z"))
            .expect("test timestamp")
    }

    fn instant(&mut self) -> Instant {
        self.clock.now()
    }

    fn new_session_id(&mut self) -> SessionId {
        let mut c = self.counters();
        c.sessions += 1;
        SessionId::new(format!("s_t{}", c.sessions))
    }

    fn new_entry_id(&mut self) -> EntryId {
        let mut c = self.counters();
        c.entries += 1;
        EntryId::new(format!("e{}", c.entries))
    }
}

/// A scripted program for [`FakePty`].
pub type Program = Arc<dyn Fn(ProgramInput<'_>, &mut ScriptedProcess<'_>) + Send + Sync>;

/// A shell that echoes what it is typed, and exits 0 on `exit\r`.
pub fn echo_shell() -> Program {
    Arc::new(|input, p| {
        if let ProgramInput::Stdin(bytes) = input {
            if bytes == b"exit\r" {
                p.exit(0);
            } else {
                p.print(bytes);
            }
        }
    })
}

pub type Runtime =
    HostRuntime<FakePty, FakeWorktrees, FakeCommands, FakeLink, FakeEventLog, TestEnv>;

/// Builds a [`HostHarness`].
pub struct HarnessBuilder {
    programs: Vec<(String, Program)>,
    log: FakeEventLog,
    env: TestEnv,
    net: FakeNetwork,
    config: RawConfig,
    worktrees: FakeWorktrees,
    commands: FakeCommands,
}

impl HarnessBuilder {
    /// Registers `program` for `command` in the fake PTY. [`SHELL`] runs [`echo_shell`] unless replaced.
    pub fn program(mut self, command: &str, program: Program) -> Self {
        self.programs.push((command.to_owned(), program));
        self
    }

    /// Starts from an existing log, as if a host had run before.
    pub fn log(mut self, log: FakeEventLog) -> Self {
        self.log = log;
        self
    }

    /// Listens on `net` instead of a network of its own, e.g. so a fake program can reach the host.
    pub fn network(mut self, net: FakeNetwork) -> Self {
        self.net = net;
        self
    }

    /// Resolves this config instead of [`shell_default_config`].
    pub fn config(mut self, config: RawConfig) -> Self {
        self.config = config;
        self
    }

    /// Uses `worktrees` (a handle on shared fake state, e.g. with repos added) instead of a fake with no repos.
    pub fn worktrees(mut self, worktrees: FakeWorktrees) -> Self {
        self.worktrees = worktrees;
        self
    }

    /// Uses `commands` (a handle on shared fake state, e.g. with scripts added) instead of a fake that knows none.
    pub fn commands(mut self, commands: FakeCommands) -> Self {
        self.commands = commands;
        self
    }

    pub fn start(self) -> HostHarness {
        let HarnessBuilder {
            programs,
            log,
            env,
            net,
            config,
            worktrees,
            commands,
        } = self;
        let dir = tempfile::tempdir().expect("tempdir for run files");
        let settings = settings(config, dir.path().join("run"));
        let runtime = build_runtime(
            &net,
            &programs,
            (worktrees.clone(), commands.clone()),
            log,
            env.clone(),
            settings.clone(),
        );
        let mut harness = HostHarness {
            runtime: Some(runtime),
            net,
            programs,
            worktrees,
            commands,
            env,
            settings,
            _dir: dir,
        };
        harness.step();
        harness
    }
}

/// Runtime settings for tests: `config` resolved with [`SHELL`], sessions in [`HOME`], run files under `run_root`.
pub fn settings(config: RawConfig, run_root: PathBuf) -> RuntimeSettings {
    RuntimeSettings {
        config: Config::resolve(Some(config), Some(SHELL.to_owned())).expect("test config"),
        home: HOME.into(),
        run_root,
        shepherd: SHEPHERD_EXE.into(),
        socket: SOCKET.into(),
    }
}

fn build_runtime(
    net: &FakeNetwork,
    programs: &[(String, Program)],
    workers: WorkerPorts,
    log: FakeEventLog,
    env: TestEnv,
    settings: RuntimeSettings,
) -> Runtime {
    let sinks = (|sink| sink, |sink| sink);
    runtime_in(net, programs, workers, log, env, sinks, settings)
}

/// A host runtime on fakes with any `EventLog` (e.g. a wrapper that counts reads), listening on `net`, its link's
/// events passing through `link_sink` (e.g. to hold some back). For tests the [`HostHarness`] doesn't cover; step
/// it with `run_until_idle`.
pub fn runtime_with<E: EventLog>(
    net: &FakeNetwork,
    programs: &[(String, Program)],
    log: E,
    env: TestEnv,
    link_sink: impl FnOnce(Arc<dyn LinkEventSink>) -> Arc<dyn LinkEventSink>,
) -> HostRuntime<FakePty, FakeWorktrees, FakeCommands, FakeLink, E, TestEnv> {
    runtime_with_sinks(net, programs, log, env, link_sink, |sink| sink)
}

/// Like [`runtime_with`], with the PTY's events passing through `pty_sink` too (e.g. to count them).
pub fn runtime_with_sinks<E: EventLog>(
    net: &FakeNetwork,
    programs: &[(String, Program)],
    log: E,
    env: TestEnv,
    link_sink: impl FnOnce(Arc<dyn LinkEventSink>) -> Arc<dyn LinkEventSink>,
    pty_sink: impl FnOnce(Arc<dyn PtyEventSink>) -> Arc<dyn PtyEventSink>,
) -> HostRuntime<FakePty, FakeWorktrees, FakeCommands, FakeLink, E, TestEnv> {
    let settings = settings(shell_default_config(), NO_RUN_ROOT.into());
    let workers = (FakeWorktrees::new(WORKTREE_ROOT), FakeCommands::new());
    let sinks = (link_sink, pty_sink);
    runtime_in(net, programs, workers, log, env, sinks, settings)
}

/// The ports that run on worker threads: `Worktrees` and `Commands`.
type WorkerPorts = (FakeWorktrees, FakeCommands);

/// The link's and the PTY's event sink wrappers.
type Sinks<FL, FP> = (FL, FP);

fn runtime_in<E: EventLog, FL, FP>(
    net: &FakeNetwork,
    programs: &[(String, Program)],
    (worktrees, commands): WorkerPorts,
    log: E,
    env: TestEnv,
    (link_sink, pty_sink): Sinks<FL, FP>,
    settings: RuntimeSettings,
) -> HostRuntime<FakePty, FakeWorktrees, FakeCommands, FakeLink, E, TestEnv>
where
    FL: FnOnce(Arc<dyn LinkEventSink>) -> Arc<dyn LinkEventSink>,
    FP: FnOnce(Arc<dyn PtyEventSink>) -> Arc<dyn PtyEventSink>,
{
    let (tx, rx) = runtime_channel();
    let mut pty = FakePty::new(pty_sink(tx.pty_sink()));
    for (command, program) in programs {
        let program = program.clone();
        pty = pty.with_program(command.clone(), move |input, p| program(input, p));
    }
    let link = net
        .listen(link_sink(tx.link_sink()))
        .expect("fake host listens");
    HostRuntime::start(pty, worktrees, commands, link, log, env, settings, tx, rx)
        .expect("host runtime starts")
}

/// A host runtime on fakes. Everything the fakes do is synchronous, so [`HostHarness::step`] runs the loop until
/// nothing is left to do.
pub struct HostHarness {
    runtime: Option<Runtime>,
    net: FakeNetwork,
    programs: Vec<(String, Program)>,
    worktrees: FakeWorktrees,
    commands: FakeCommands,
    env: TestEnv,
    settings: RuntimeSettings,
    /// Holds the run root; removed with the harness.
    _dir: tempfile::TempDir,
}

impl HostHarness {
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder {
            programs: vec![(SHELL.to_owned(), echo_shell())],
            log: FakeEventLog::new(),
            env: TestEnv::new(),
            net: FakeNetwork::new(),
            config: shell_default_config(),
            worktrees: FakeWorktrees::new(WORKTREE_ROOT),
            commands: FakeCommands::new(),
        }
    }

    pub fn new() -> Self {
        Self::builder().start()
    }

    pub fn runtime(&self) -> &Runtime {
        self.runtime.as_ref().expect("runtime")
    }

    pub fn runtime_mut(&mut self) -> &mut Runtime {
        self.runtime.as_mut().expect("runtime")
    }

    /// The live host state.
    pub fn host(&self) -> &Host {
        self.runtime().host()
    }

    pub fn log(&self) -> &FakeEventLog {
        self.runtime().log()
    }

    pub fn clock(&self) -> Clock {
        self.env.clock()
    }

    /// Where the host keeps each session's run files, in `<run_root>/<session>/`. Created by the host when it
    /// first needs it.
    pub fn run_root(&self) -> &Path {
        &self.settings.run_root
    }

    /// Runs the loop until no event is pending, no deadline is due, and no port call on a worker thread is
    /// unanswered. Panics if a worker call doesn't answer within a generous liveness bound.
    pub fn step(&mut self) {
        let settled = self.runtime_mut().run_until_settled(WORKER_LIMIT);
        assert!(settled, "a worker call never answered");
    }

    /// Runs the loop until no event is pending and no deadline is due, without waiting for port calls running on a
    /// worker thread: for a test that holds one of them.
    pub fn poll(&mut self) {
        self.runtime_mut().run_until_idle();
    }

    /// Feeds one event into the loop's channel, then steps.
    pub fn send(&mut self, event: RuntimeEvent) {
        self.runtime().sender().send(event);
        self.step();
    }

    /// Connects a client and steps, so the host has seen it.
    pub fn connect(&mut self) -> FakeClient {
        let client = FakeClient::connect(&self.net);
        self.step();
        client
    }

    /// Connects a slow client: it reads only what [`FakeClient::read`] lets it.
    pub fn connect_paced(&mut self) -> FakeClient {
        let client = FakeClient::connect_with(&self.net, true);
        self.step();
        client
    }

    /// Stops this host (dropping its link, so clients see it go) and starts a new one on the same log, as if the
    /// process restarted. PTYs don't survive: the fake PTY is dropped with the old runtime.
    pub fn restart(&mut self) {
        let old = self.runtime.take().expect("runtime");
        let log = old.into_log();
        let runtime = build_runtime(
            &self.net,
            &self.programs,
            (self.worktrees.clone(), self.commands.clone()),
            log,
            self.env.clone(),
            self.settings.clone(),
        );
        self.runtime = Some(runtime);
        self.step();
    }

    /// [`HostHarness::restart`], with the config changed to `config`, as if the user edited it meanwhile.
    pub fn restart_with_config(&mut self, config: RawConfig) {
        let run_root = self.settings.run_root.clone();
        self.settings = settings(config, run_root);
        self.restart();
    }

    /// A fresh `Host` folded from every entry in the log, as a restarted host would build it.
    pub fn replayed(&self) -> Host {
        let log = self.log();
        let mut host = Host::new();
        for id in log.logs().expect("logs") {
            for env in log.read(id, Seq::ZERO).expect("read") {
                host.apply(&env);
            }
        }
        host
    }
}

struct ChannelSink(Mutex<Sender<LinkEvent>>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

/// A client on the fake network: sends requests and notifications, and reads what the host sent.
pub struct FakeClient {
    link: FakeLink,
    net: FakeNetwork,
    conn: ConnId,
    events: Receiver<LinkEvent>,
    next_id: i64,
    /// What arrived and was not yet taken.
    inbox: Vec<HostMessage>,
    disconnected: bool,
}

impl FakeClient {
    fn connect(net: &FakeNetwork) -> Self {
        Self::connect_with(net, false)
    }

    pub fn connect_with(net: &FakeNetwork, paced: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(ChannelSink(Mutex::new(tx)));
        let (link, conn) = if paced {
            net.connect_paced(sink)
        } else {
            net.connect(sink)
        }
        .expect("connect to fake host");
        Self {
            link,
            net: net.clone(),
            conn,
            events: rx,
            next_id: 0,
            inbox: Vec::new(),
            disconnected: false,
        }
    }

    /// A paced client reads only through this: up to `max_bytes` of what the host sent (at least one line, if
    /// any is waiting). Returns the bytes read.
    pub fn read(&mut self, max_bytes: usize) -> usize {
        self.net.deliver(self.conn, max_bytes)
    }

    /// Sends a request and returns its id.
    pub fn request(&mut self, body: ClientRequest) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        let line = encode_request(&Request {
            id: id.clone(),
            body,
        });
        self.send_line(&line);
        id
    }

    pub fn notify(&mut self, note: ClientNotification) {
        self.send_line(&encode_client_notification(&note));
    }

    /// Sends a raw line (without its `\n`).
    pub fn send_raw(&mut self, line: &[u8]) {
        self.link.send(self.conn, line).expect("send to host");
    }

    fn send_line(&mut self, line: &[u8]) {
        self.send_raw(line.strip_suffix(b"\n").unwrap_or(line));
    }

    pub fn new_session(&mut self) -> RequestId {
        self.request(ClientRequest::SessionNew(SessionNew::default()))
    }

    pub fn subscribe(&mut self, from: SubscribeFrom) -> RequestId {
        self.request(ClientRequest::Subscribe(Subscribe { from }))
    }

    pub fn subscribe_all(&mut self) -> RequestId {
        self.subscribe(SubscribeFrom::default())
    }

    /// Keys typed into `session`.
    pub fn input(&mut self, session: &SessionId, data: &[u8]) {
        self.input_of(session, InputKind::Keys, data);
    }

    /// Input of `kind` for `session`.
    pub fn input_of(&mut self, session: &SessionId, kind: InputKind, data: &[u8]) {
        self.notify(ClientNotification::SessionInput(SessionInput {
            session: session.clone(),
            data: data.to_vec(),
            kind,
        }));
    }

    pub fn resize(&mut self, session: &SessionId, cols: u16, rows: u16) {
        self.notify(ClientNotification::SessionResize(SessionResize {
            session: session.clone(),
            cols,
            rows,
        }));
    }

    fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                LinkEvent::Line { line, .. } => {
                    let msg = decode_host_message(&line).expect("host sent a valid message");
                    self.inbox.push(msg);
                }
                LinkEvent::Disconnected { .. } => self.disconnected = true,
                LinkEvent::Connected { .. } => {}
            }
        }
    }

    /// Everything received since the last call, in order.
    pub fn take(&mut self) -> Vec<HostMessage> {
        self.pump();
        std::mem::take(&mut self.inbox)
    }

    pub fn is_disconnected(&mut self) -> bool {
        self.pump();
        self.disconnected
    }
}

/// The envelopes among `msgs`, in order.
pub fn entries(msgs: &[HostMessage]) -> Vec<Envelope> {
    msgs.iter()
        .filter_map(|m| match m {
            HostMessage::Notification(HostNotification::LogEntry(e)) => Some(e.envelope.clone()),
            _ => None,
        })
        .collect()
}

/// The PTY output bytes of `session` among `msgs`, concatenated.
pub fn output(msgs: &[HostMessage], session: &SessionId) -> Vec<u8> {
    msgs.iter()
        .filter_map(|m| match m {
            HostMessage::Notification(HostNotification::PtyOutput(o)) if &o.session == session => {
                Some(o.bytes.clone())
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// The `result` of the response to `id`, untyped. Panics if there is none.
pub fn response(
    msgs: &[HostMessage],
    id: &RequestId,
) -> Result<serde_json::Value, shepherd_core::wire::WireError> {
    msgs.iter()
        .find_map(|m| match m {
            HostMessage::Response(r) if &r.id == id => Some(r.result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no response to {id}"))
}

/// The session id in the response to a `session/new` request `id`.
pub fn new_session_id(msgs: &[HostMessage], id: &RequestId) -> SessionId {
    let result = response(msgs, id).expect("session/new succeeded");
    SessionId::new(result["session"].as_str().expect("session id"))
}

/// Folds `envelopes` into a `Host`, the way a client can rebuild the session list from log entries.
pub fn fold(envelopes: &[Envelope]) -> Host {
    let mut host = Host::new();
    for env in envelopes {
        host.apply(env);
    }
    host
}
