//! The host runtime loop (decision 0011): one thread draining one unbounded channel of [`RuntimeEvent`]s. For each
//! event it calls [`Host::handle`] and executes the commands in order through the ports, following the runtime
//! contract documented on [`Host`].
//!
//! Besides what core decides, the runtime owns three things that aren't domain state:
//! - **Screens.** One `shepherd_term::Screen` per live PTY, fed every output chunk. Its query answers are written
//!   back to the PTY, so agents get them with no client attached (decision 0007). A synchronized update's hold is
//!   serviced as a deadline.
//! - **Subscriptions.** Per connection, paced by the link's backpressure (decision 0013). A subscribe starts a
//!   cursor per log; entries are read from the log and sent while the conn's queue is below [`LOW_WATER`]. A
//!   blocked catch-up retries after [`CATCH_UP_TICK`], doubling up to [`MAX_CATCH_UP_TICK`] while its queue doesn't
//!   shrink. Entries are read [`CATCH_UP_BATCH`] at a time (`EventLog::read_batch`), so each is read once and a
//!   conn holds at most one batch. A conn whose queue hasn't shrunk for [`STALL_TIMEOUT`] is
//!   closed: a client that stopped reading. A log whose cursor hasn't reached its end gets no live
//!   entries (the cursor reads them); once it reaches the end, in that same iteration, the log goes live. When
//!   every cursor is done the conn gets a `snapshot_ansi()` of every live screen, then the ack, and from then on
//!   live PTY output too. Appends only happen on this thread, so there is no gap and no duplicate.
//! - **Replies.** Each request turned into a [`HostEvent`] gets a [`ReplyTo`] that maps back to its connection and
//!   JSON-RPC id.
//! - **Run files.** A new session's launch (`launch::plan`, from its profile) may come with files, e.g. Claude
//!   Code's generated settings. They are written into the session's run directory just before its PTY is spawned
//!   (a failure to write them fails the spawn), and the directory is removed when its PTY's exit or its
//!   `session/closed` is appended. At startup, when no PTY is running, every leftover run directory is swept.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use shepherd_core::acpp::{
    Envelope, Frame, FrameDraft, RequestId, Seq, ShepherdFrame, SourceKind, Stamp,
};
use shepherd_core::agents::Translators;
use shepherd_core::config::{AgentKind, Config};
use shepherd_core::host::launch::{self, LaunchContext, RunFile};
use shepherd_core::host::{Host, HostCommand, HostEvent, NewSessionSpec, ReplyTo};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    ConnId, EventLog, Link, LinkEvent, LinkEventSink, LogError, LogId, MAX_QUEUED, Pty, PtyEvent,
    PtyEventSink, PtySize,
};
use shepherd_core::wire::{
    self, ClientMessage, ClientNotification, ClientRequest, HookFrame, HostNotification,
    HostResponse, HostResult, LogEntry, PtyOutput, SessionNew, SubscribeFrom, WireError,
};
use shepherd_term::Screen;

use super::env::Env;
use super::run_files;
use crate::screen_guard;

/// The machine id every entry is stamped with until machines get names (T29).
pub const LOCAL_MACHINE: &str = "local";
/// The size a new session's PTY starts at, until a client resizes it.
pub const INITIAL_SIZE: PtySize = PtySize { cols: 80, rows: 24 };
/// How long shutdown waits for killed PTYs to report their exit before giving up on them.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// A subscription's catch-up only sends while the conn has fewer bytes than this queued, far below the
/// [`MAX_QUEUED`] bound past which the link drops it.
pub const LOW_WATER: usize = MAX_QUEUED / 4;
/// How soon a blocked catch-up first checks whether its conn has drained.
pub const CATCH_UP_TICK: Duration = Duration::from_millis(5);
/// The longest wait between checks of a catch-up whose conn isn't draining.
pub const MAX_CATCH_UP_TICK: Duration = Duration::from_millis(500);
/// A catching-up conn whose queue hasn't shrunk for this long is closed.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How many entries a catch-up reads from the log at a time, and the most it holds per conn.
pub const CATCH_UP_BATCH: usize = 256;

/// Everything the loop reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Pty(PtyEvent),
    Link(LinkEvent),
    /// SIGTERM or SIGINT: kill the PTYs, record their exits, close the links, stop.
    Shutdown,
}

/// The sending side of the loop's channel. Unbounded, so a port's sink never blocks on the loop that owns the
/// port (which could be inside `Link::close`, waiting for that very sink).
#[derive(Clone)]
pub struct RuntimeTx {
    tx: Sender<RuntimeEvent>,
    /// Set by [`RuntimeEvent::Shutdown`], so the loop sees it on its next iteration instead of behind whatever is
    /// already queued.
    shutdown: Arc<AtomicBool>,
}

impl RuntimeTx {
    /// Queues `event`. Never blocks. Dropped silently once the loop has stopped. A `Shutdown` jumps the queue.
    pub fn send(&self, event: RuntimeEvent) {
        if event == RuntimeEvent::Shutdown {
            self.shutdown.store(true, Ordering::SeqCst);
        }
        let _ = self.tx.send(event);
    }

    /// A sink for a [`Pty`] adapter, forwarding into this channel.
    pub fn pty_sink(&self) -> Arc<dyn PtyEventSink> {
        Arc::new(self.clone())
    }

    /// A sink for a [`Link`] adapter, forwarding into this channel.
    pub fn link_sink(&self) -> Arc<dyn LinkEventSink> {
        Arc::new(self.clone())
    }
}

impl PtyEventSink for RuntimeTx {
    fn send(&self, event: PtyEvent) {
        RuntimeTx::send(self, RuntimeEvent::Pty(event));
    }
}

impl LinkEventSink for RuntimeTx {
    fn send(&self, event: LinkEvent) {
        RuntimeTx::send(self, RuntimeEvent::Link(event));
    }
}

/// The receiving side, owned by the runtime.
pub struct RuntimeRx(Receiver<RuntimeEvent>);

/// A new channel for one runtime. Build the port sinks from the sender before building the ports.
pub fn runtime_channel() -> (RuntimeTx, RuntimeRx) {
    let (tx, rx) = mpsc::channel();
    let tx = RuntimeTx {
        tx,
        shutdown: Arc::default(),
    };
    (tx, RuntimeRx(rx))
}

/// What the runtime needs besides its ports.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub config: Config,
    /// Where new sessions start.
    pub home: PathBuf,
    /// Each session's run files go in `<run_root>/<session>/` (`~/.shepherd/run`).
    pub run_root: PathBuf,
    /// The running `shepherd` executable, absolute: what agents' hooks run.
    pub shepherd: PathBuf,
    /// The host socket, for `shepherd hook`.
    pub socket: PathBuf,
}

#[derive(Debug, Default)]
struct ConnState {
    sub: Option<Subscription>,
}

/// One conn's subscription.
#[derive(Debug)]
struct Subscription {
    /// Logs still catching up. Their entries are sent by the cursor, never live.
    cursors: BTreeMap<LogId, Cursor>,
    /// The subscribe request, answered when catch-up completes.
    ack: RequestId,
    /// Every cursor is done and the snapshots are sent: everything is live, PTY output included.
    complete: bool,
    /// Set while the catch-up is blocked on a full queue.
    blocked: Option<Blocked>,
}

/// A catch-up waiting for its conn to drain.
#[derive(Debug, Clone, Copy)]
struct Blocked {
    /// The queue at the last check. A smaller one at the next check is progress, even if live traffic grew it in
    /// between.
    queued: usize,
    /// The last progress, or when it blocked.
    since: Instant,
    /// When to check again, and how long the wait before that was.
    retry_at: Instant,
    wait: Duration,
}

#[derive(Debug)]
struct Cursor {
    /// The last seq sent.
    after: Seq,
    /// Entries read from the log and not sent yet, at most [`CATCH_UP_BATCH`]. Refilled from `after` when empty;
    /// empty after a refill means the cursor has reached the end.
    batch: VecDeque<Envelope>,
}

impl ConnState {
    /// Whether this conn gets a new entry of `log` live.
    fn live_for(&self, log: &LogId) -> bool {
        self.sub
            .as_ref()
            .is_some_and(|s| !s.cursors.contains_key(log))
    }

    fn output_live(&self) -> bool {
        self.sub.as_ref().is_some_and(|s| s.complete)
    }
}

/// A screen's open synchronized-update hold and when it times out.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    hold: u64,
    due: Instant,
}

/// The host daemon's loop, generic over its ports so scenario tests run it on fakes.
pub struct HostRuntime<P, L, E, V> {
    host: Host,
    pty: P,
    link: L,
    log: E,
    env: V,
    settings: RuntimeSettings,
    machine: MachineId,
    tx: RuntimeTx,
    rx: RuntimeRx,
    screens: BTreeMap<SessionId, Screen>,
    deadlines: HashMap<SessionId, Deadline>,
    conns: BTreeMap<ConnId, ConnState>,
    replies: HashMap<ReplyTo, (ConnId, RequestId)>,
    next_reply: u64,
    /// Per-session hook translation state, dropped when the session closes. Not host state: a restarted host starts
    /// with none.
    translators: Translators,
    /// Seeds every translator this host run creates; made on the first hook.
    hook_seed: Option<String>,
    /// Run files of sessions being created, written just before their PTY is spawned.
    pending_run_files: HashMap<SessionId, Vec<RunFile>>,
    /// Set once shutdown starts: when it gives up waiting for PTY exits.
    shutdown_by: Option<Instant>,
    finished: bool,
}

impl<P: Pty, L: Link, E: EventLog, V: Env> HostRuntime<P, L, E, V> {
    /// Rebuilds the host state by folding every log, then records an exit for every session that was still
    /// running: PTYs never survive a host restart. `tx` and `rx` are the channel the ports' sinks feed.
    pub fn start(
        pty: P,
        link: L,
        log: E,
        env: V,
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
    ) -> Result<Self, LogError> {
        let mut host = Host::new();
        for id in log.logs()? {
            for envelope in log.read(id, Seq::ZERO)? {
                host.apply(&envelope);
            }
        }
        let mut runtime = Self {
            host,
            pty,
            link,
            log,
            env,
            settings,
            machine: MachineId::new(LOCAL_MACHINE),
            tx,
            rx,
            screens: BTreeMap::new(),
            deadlines: HashMap::new(),
            conns: BTreeMap::new(),
            replies: HashMap::new(),
            next_reply: 0,
            translators: Translators::new(),
            hook_seed: None,
            pending_run_files: HashMap::new(),
            shutdown_by: None,
            finished: false,
        };
        let orphans: Vec<SessionId> = runtime
            .host
            .sessions()
            .into_iter()
            .filter(|s| !s.exited)
            .map(|s| s.id)
            .collect();
        for session in orphans {
            runtime.dispatch(HostEvent::PtyExited {
                session,
                code: None,
            });
        }
        // Every run directory is stale: the log lock is held and nothing has been spawned yet.
        run_files::sweep(&runtime.settings.run_root);
        Ok(runtime)
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn log(&self) -> &E {
        &self.log
    }

    pub fn translators(&self) -> &Translators {
        &self.translators
    }

    /// How many sessions have run files waiting for their spawn. Zero between requests.
    pub fn pending_run_files(&self) -> usize {
        self.pending_run_files.len()
    }

    pub fn pty(&self) -> &P {
        &self.pty
    }

    pub fn pty_mut(&mut self) -> &mut P {
        &mut self.pty
    }

    /// The loop's own sender, e.g. for a signal handler.
    pub fn sender(&self) -> RuntimeTx {
        self.tx.clone()
    }

    /// Shutdown has completed: the links are closed and the loop has nothing left to do.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Gives the log back, e.g. to start another runtime on it (a restart, in tests).
    pub fn into_log(self) -> E {
        self.log
    }

    /// The earliest deadline the loop has to wake up for, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadlines
            .values()
            .map(|d| d.due)
            .chain(
                self.conns
                    .values()
                    .filter_map(|c| c.sub.as_ref().and_then(|s| s.blocked).map(|b| b.retry_at)),
            )
            .chain(self.shutdown_by)
            .min()
    }

    /// Runs until shutdown completes, sleeping in `recv_timeout` until the next event or deadline. A shutdown
    /// request and due deadlines are serviced on every iteration, so a busy channel can't starve them.
    pub fn run(&mut self) {
        loop {
            self.service();
            if self.finished {
                break;
            }
            let next = match self.next_deadline() {
                None => self.rx.0.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(due) => {
                    let wait = due.saturating_duration_since(self.env.instant());
                    self.rx.0.recv_timeout(wait)
                }
            };
            match next {
                Ok(event) => self.handle(event),
                Err(RecvTimeoutError::Timeout) => {}
                // The runtime holds a sender, so this can't happen; stop rather than spin.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Handles every event already queued and every deadline already due, until there is none. For tests, where
    /// the fakes queue their events synchronously.
    pub fn run_until_idle(&mut self) {
        loop {
            self.service();
            match self.rx.0.try_recv() {
                Ok(event) => self.handle(event),
                Err(_) => break,
            }
        }
    }

    /// Handles one event.
    pub fn handle(&mut self, event: RuntimeEvent) {
        if self.finished {
            return;
        }
        match event {
            // Output while shutting down is dropped unread, so a flooding PTY's backlog drains fast and the exits
            // queued behind it are reached.
            RuntimeEvent::Pty(PtyEvent::Output { .. }) if self.shutdown_by.is_some() => {}
            RuntimeEvent::Pty(PtyEvent::Output { session, bytes }) => {
                self.pty_output(session, bytes)
            }
            RuntimeEvent::Pty(PtyEvent::Exited { session, code }) => {
                self.screens.remove(&session);
                self.deadlines.remove(&session);
                self.dispatch(HostEvent::PtyExited { session, code });
                self.check_shutdown_done();
            }
            RuntimeEvent::Link(LinkEvent::Connected { conn }) => {
                self.conns.insert(conn, ConnState::default());
            }
            RuntimeEvent::Link(LinkEvent::Line { conn, line }) => {
                if self.shutdown_by.is_none() {
                    self.client_line(conn, &line);
                }
            }
            RuntimeEvent::Link(LinkEvent::Disconnected { conn, reason }) => {
                tracing::debug!(%conn, ?reason, "client disconnected");
                self.forget(conn);
            }
            RuntimeEvent::Shutdown => self.start_shutdown(),
        }
    }

    // ---------- core events and commands ----------

    /// Runs `event` through core and executes what it returns, then any events that execution produced.
    fn dispatch(&mut self, event: HostEvent) {
        let mut queue = VecDeque::from([event]);
        while let Some(event) = queue.pop_front() {
            let commands = self.host.handle(event);
            self.execute(commands, &mut queue);
        }
    }

    /// Executes `commands` in order. Events they produce (a failed spawn) are pushed to `follow_up`.
    fn execute(&mut self, commands: Vec<HostCommand>, follow_up: &mut VecDeque<HostEvent>) {
        let mut commands = commands.into_iter();
        while let Some(command) = commands.next() {
            match command {
                HostCommand::Append(draft) => {
                    if let Err(error) = self.append(*draft) {
                        self.abandon(&error, commands.by_ref());
                        return;
                    }
                }
                HostCommand::TranslateHook {
                    session,
                    agent,
                    payload,
                } => {
                    if let Err(error) = self.translate_hook(&session, agent, &payload) {
                        self.abandon(&error, commands.by_ref());
                        return;
                    }
                }
                HostCommand::SpawnPty { session, spec } => {
                    let size = spec.size;
                    let spawned = self.write_run_files(&session).and_then(|()| {
                        self.pty
                            .spawn(session.clone(), spec)
                            .map_err(|e| e.to_string())
                    });
                    match spawned {
                        Ok(()) => {
                            self.screens
                                .insert(session, Screen::new(size.cols, size.rows));
                        }
                        Err(error) => {
                            tracing::warn!(%session, %error, "spawn failed");
                            follow_up.push_back(HostEvent::PtyExited {
                                session,
                                code: None,
                            });
                        }
                    }
                }
                HostCommand::WritePty { session, bytes } => self.write_pty(&session, &bytes),
                HostCommand::ResizePty { session, size } => {
                    if let Err(error) = self.pty.resize(&session, size) {
                        tracing::warn!(%session, %error, "resize failed");
                    }
                    if let Some(screen) = self.screens.get_mut(&session) {
                        screen.resize(size.cols, size.rows);
                    }
                }
                HostCommand::KillPty { session } => {
                    if let Err(error) = self.pty.kill(&session) {
                        tracing::warn!(%session, %error, "kill failed");
                    }
                }
                HostCommand::Reply { to, result } => self.reply(to, result),
            }
        }
    }

    /// After a failed append: skips the rest of the batch, answering its replies with an error.
    fn abandon(&mut self, error: &LogError, rest: impl Iterator<Item = HostCommand>) {
        tracing::error!(%error, "append failed; skipping the rest of the batch");
        for command in rest {
            if let HostCommand::Reply { to, .. } = command {
                let error = WireError::Internal(format!("append failed: {error}"));
                self.reply(to, Err(error));
            }
        }
    }

    /// Translates a hook payload with the session's translator and appends what it gives. A session's first hook
    /// in this host run creates its translator, seeded with an entry id made once per host run: globally unique,
    /// so its ids never collide with those of a translator from an earlier run still in the log.
    fn translate_hook(
        &mut self,
        session: &SessionId,
        agent: AgentKind,
        payload: &Value,
    ) -> Result<(), LogError> {
        let env = &mut self.env;
        let seed = &mut self.hook_seed;
        let drafts = self.translators.translate(session, agent, payload, || {
            seed.get_or_insert_with(|| env.new_entry_id().as_str().to_owned())
                .clone()
        });
        for draft in drafts {
            self.append(draft)?;
        }
        Ok(())
    }

    /// Seals and appends `draft`, applies the envelope, and fans it out to subscribers.
    fn append(&mut self, draft: FrameDraft) -> Result<(), LogError> {
        let stamp = Stamp {
            id: self.env.new_entry_id(),
            ts: self.env.now(),
            machine: self.machine.clone(),
        };
        let log = LogId::of(&draft);
        let envelope = self.log.append(log.clone(), draft, stamp)?;
        self.host.apply(&envelope);
        self.release_if_ended(&envelope);
        let line = log_entry_line(envelope);
        let to: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.live_for(&log))
            .map(|(&conn, _)| conn)
            .collect();
        for conn in to {
            self.send(conn, &line);
        }
        Ok(())
    }

    fn write_pty(&mut self, session: &SessionId, bytes: &[u8]) {
        if let Err(error) = self.pty.write(session, bytes) {
            tracing::warn!(%session, %error, "write failed");
        }
    }

    // ---------- PTY output and screens ----------

    fn pty_output(&mut self, session: SessionId, bytes: Vec<u8>) {
        if let Some(screen) = self.screens.get_mut(&session) {
            let fed = screen_guard::update(screen, |s| s.feed(&bytes));
            log_screen_panic(&session, fed);
            self.after_feed(&session);
        }
        let line = notification_line(&HostNotification::PtyOutput(PtyOutput { session, bytes }));
        let to: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.output_live())
            .map(|(&conn, _)| conn)
            .collect();
        for conn in to {
            self.send(conn, &line);
        }
    }

    /// Writes the screen's query answers back to its PTY and keeps its sync-update deadline current.
    fn after_feed(&mut self, session: &SessionId) {
        let Some(screen) = self.screens.get_mut(session) else {
            return;
        };
        let replies = screen.take_replies();
        let hold = screen.sync_hold();
        if !replies.is_empty() {
            self.write_pty(session, &replies);
        }
        match hold {
            Some(hold) => {
                if self.deadlines.get(session).map(|d| d.hold) != Some(hold.id) {
                    let due = self.env.instant() + hold.timeout;
                    self.deadlines
                        .insert(session.clone(), Deadline { hold: hold.id, due });
                }
            }
            None => {
                self.deadlines.remove(session);
            }
        }
    }

    /// What every loop iteration does before taking the next event: fire due deadlines, then start a requested
    /// shutdown.
    fn service(&mut self) {
        self.fire_due();
        if self.tx.shutdown.load(Ordering::SeqCst) {
            self.start_shutdown();
        }
    }

    /// Ends every synchronized update whose deadline has passed, resumes pending catch-ups, and finishes a
    /// shutdown that ran out of time.
    fn fire_due(&mut self) {
        let now = self.env.instant();
        let retry: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.sub
                    .as_ref()
                    .and_then(|s| s.blocked)
                    .is_some_and(|b| b.retry_at <= now)
            })
            .map(|(&conn, _)| conn)
            .collect();
        for conn in retry {
            self.pump(conn);
        }
        let due: Vec<(SessionId, u64)> = self
            .deadlines
            .iter()
            .filter(|(_, d)| d.due <= now)
            .map(|(s, d)| (s.clone(), d.hold))
            .collect();
        for (session, hold) in due {
            self.deadlines.remove(&session);
            if let Some(screen) = self.screens.get_mut(&session) {
                let expired = screen_guard::update(screen, |s| s.expire_sync(hold));
                log_screen_panic(&session, expired);
                self.after_feed(&session);
            }
        }
        if self.shutdown_by.is_some_and(|by| by <= now) && !self.finished {
            tracing::warn!(
                left = self.screens.len(),
                "PTYs didn't exit in time; stopping anyway"
            );
            self.finish();
        }
    }

    // ---------- clients ----------

    fn client_line(&mut self, conn: ConnId, line: &[u8]) {
        match wire::decode_client_message(line) {
            Err(error) => {
                tracing::debug!(%conn, %error, "bad client message");
                if let Some(response) = error.into_response() {
                    self.respond(conn, &response);
                }
            }
            Ok(ClientMessage::Notification(note)) => match note {
                ClientNotification::SessionInput(input) => self.dispatch(HostEvent::Input {
                    session: input.session,
                    bytes: input.data,
                }),
                ClientNotification::SessionResize(resize) => self.dispatch(HostEvent::Resize {
                    session: resize.session,
                    size: PtySize {
                        cols: resize.cols,
                        rows: resize.rows,
                    },
                }),
            },
            Ok(ClientMessage::Request(request)) => {
                self.client_request(conn, request.id, request.body)
            }
        }
    }

    fn client_request(&mut self, conn: ConnId, id: RequestId, body: ClientRequest) {
        let respond_now = |result| HostResponse {
            id: id.clone(),
            result,
        };
        match body {
            ClientRequest::Hello(hello) => {
                let result = wire::negotiate(&hello, &self.machine)
                    .map(|ok| HostResult::Hello(ok.with_profiles(&self.settings.config)));
                self.respond(conn, &respond_now(result));
            }
            ClientRequest::Subscribe(subscribe) => self.subscribe(conn, id, &subscribe.from),
            ClientRequest::HookFrame(hook) => {
                let reply = self.reply_to(conn, id);
                let HookFrame {
                    session,
                    agent,
                    payload,
                } = *hook;
                self.dispatch(HostEvent::Hook {
                    reply,
                    session,
                    agent,
                    payload,
                });
            }
            ClientRequest::SessionNew(new) => {
                let reply = self.reply_to(conn, id);
                let session = self.env.new_session_id();
                let (spec, files) = match self.plan_session(&session, &new) {
                    Ok(planned) => planned,
                    Err(error) => return self.reply(reply, Err(error)),
                };
                if !files.is_empty() {
                    self.pending_run_files.insert(session.clone(), files);
                }
                let event = HostEvent::NewSession {
                    reply,
                    id: session.clone(),
                    at: self.env.now(),
                    spec,
                };
                self.dispatch(event);
                // Left over only if core refused the session and nothing was spawned.
                self.pending_run_files.remove(&session);
            }
            ClientRequest::SessionRename(rename) => {
                let reply = self.reply_to(conn, id);
                self.dispatch(HostEvent::Rename {
                    reply,
                    session: rename.session,
                    name: rename.name,
                });
            }
            ClientRequest::SessionClose(close) => {
                let reply = self.reply_to(conn, id);
                self.dispatch(HostEvent::Close {
                    reply,
                    session: close.session,
                });
            }
        }
    }

    /// What a new session runs, and the run files to write before spawning it: the named profile, else
    /// `[defaults] agent`, in the requested directory, else the home directory, at [`INITIAL_SIZE`].
    fn plan_session(
        &self,
        session: &SessionId,
        new: &SessionNew,
    ) -> Result<(NewSessionSpec, Vec<RunFile>), WireError> {
        let config = &self.settings.config;
        let name = new.profile.as_deref();
        let profile = config.profile_or_default(name).ok_or_else(|| {
            WireError::InvalidParams(format!("unknown profile `{}`", name.unwrap_or_default()))
        })?;
        let cwd = match new.cwd.as_deref() {
            Some(cwd) => existing_dir(cwd)?,
            None => self.settings.home.clone(),
        };
        let ctx = LaunchContext {
            session: session.clone(),
            cwd,
            size: INITIAL_SIZE,
            run_dir: run_files::session_dir(&self.settings.run_root, session),
            shepherd: self.settings.shepherd.clone(),
            socket: self.settings.socket.clone(),
        };
        let launch = launch::plan(profile, &ctx).map_err(|e| WireError::Internal(e.to_string()))?;
        let spec = NewSessionSpec {
            name: None,
            profile: profile.name.clone(),
            agent: profile.kind,
            spawn: launch.spawn,
        };
        Ok((spec, launch.files))
    }

    /// Writes the run files waiting for `session`'s spawn, if any.
    fn write_run_files(&mut self, session: &SessionId) -> Result<(), String> {
        let Some(files) = self.pending_run_files.remove(session) else {
            return Ok(());
        };
        run_files::write(&self.settings.run_root, session, &files)
            .map_err(|e| format!("cannot write run files: {e}"))
    }

    /// Releases what a session holds for this host run once it has ended. Its run files go when its PTY's
    /// `agent/exited` (source `shepherd`) or its `session/closed` is in the log: only the agent reads them. Its hook
    /// translator goes when it closes: a closed session takes no more hooks.
    fn release_if_ended(&mut self, envelope: &Envelope) {
        let (Some(session), Frame::Shepherd(message)) = (envelope.session(), envelope.frame())
        else {
            return;
        };
        match message.body() {
            ShepherdFrame::SessionClosed(_) => {
                self.translators.remove(session);
            }
            ShepherdFrame::AgentExited(_) if envelope.source().kind == SourceKind::Shepherd => {}
            _ => return,
        }
        if let Err(error) = run_files::remove(&self.settings.run_root, session) {
            tracing::warn!(%session, %error, "cannot remove run files");
        }
    }

    /// Starts a subscription: a cursor for every log, from where the client left off. The catch-up starts in this
    /// iteration and is answered when it completes (see the module docs). A newer subscribe on the same conn
    /// replaces an unfinished one, whose request is answered with an error.
    fn subscribe(&mut self, conn: ConnId, id: RequestId, from: &SubscribeFrom) {
        let logs = match self.log.logs() {
            Ok(logs) => logs,
            Err(e) => {
                let error = WireError::Internal(format!("reading the log: {e}"));
                self.respond(
                    conn,
                    &HostResponse {
                        id,
                        result: Err(error),
                    },
                );
                return;
            }
        };
        let cursors = logs
            .into_iter()
            .map(|log| {
                let after = match &log {
                    LogId::Host => from.host.unwrap_or(Seq::ZERO),
                    LogId::Session(s) => from.sessions.get(s).copied().unwrap_or(Seq::ZERO),
                };
                let cursor = Cursor {
                    after,
                    batch: VecDeque::new(),
                };
                (log, cursor)
            })
            .collect();
        let sub = Subscription {
            cursors,
            ack: id,
            complete: false,
            blocked: None,
        };
        let replaced = self.conns.entry(conn).or_default().sub.replace(sub);
        if let Some(old) = replaced.filter(|old| !old.complete) {
            let error = WireError::InvalidRequest("replaced by a newer subscribe".to_owned());
            self.respond(
                conn,
                &HostResponse {
                    id: old.ack,
                    result: Err(error),
                },
            );
        }
        self.pump(conn);
    }

    /// Sends `conn`'s catch-up while its queue is below [`LOW_WATER`]. Finishes the subscription when every cursor
    /// is done, or schedules another try.
    fn pump(&mut self, conn: ConnId) {
        loop {
            let Some(sub) = self.conns.get_mut(&conn).and_then(|c| c.sub.as_mut()) else {
                return;
            };
            if sub.complete {
                return;
            }
            let Some(mut entry) = sub.cursors.first_entry() else {
                self.complete(conn);
                return;
            };
            match self.link.queued_bytes(conn) {
                // Gone, its `Disconnected` still on the way: forget it now, so no retry deadline is left behind.
                None => {
                    self.forget(conn);
                    return;
                }
                Some(queued) if queued >= LOW_WATER => {
                    self.block(conn, queued);
                    return;
                }
                Some(_) => {}
            }
            sub.blocked = None;
            let log = entry.key().clone();
            let cursor = entry.get_mut();
            if cursor.batch.is_empty() {
                match self.log.read_batch(log, cursor.after, CATCH_UP_BATCH) {
                    // The end of this log: it goes live now, before anything else can be appended to it.
                    Ok(read) if read.is_empty() => {
                        entry.remove();
                    }
                    Ok(read) => cursor.batch = read.into(),
                    Err(e) => {
                        tracing::error!(%conn, error = %e, "subscribe catch-up failed");
                        let ack = sub.ack.clone();
                        if let Some(c) = self.conns.get_mut(&conn) {
                            c.sub = None;
                        }
                        let error = WireError::Internal(format!("reading the log: {e}"));
                        self.respond(
                            conn,
                            &HostResponse {
                                id: ack,
                                result: Err(error),
                            },
                        );
                        return;
                    }
                }
                continue;
            }
            if let Some(envelope) = cursor.batch.pop_front() {
                cursor.after = envelope.seq();
                self.send(conn, &log_entry_line(envelope));
            }
        }
    }

    /// `conn`'s catch-up found `queued` bytes waiting. Schedules the next check, backing off while the queue
    /// doesn't shrink, and closes the conn once it hasn't shrunk for [`STALL_TIMEOUT`].
    fn block(&mut self, conn: ConnId, queued: usize) {
        let now = self.env.instant();
        let Some(sub) = self.conns.get_mut(&conn).and_then(|c| c.sub.as_mut()) else {
            return;
        };
        let blocked = match sub.blocked {
            Some(b) if queued >= b.queued => {
                if now.saturating_duration_since(b.since) >= STALL_TIMEOUT {
                    tracing::warn!(%conn, queued, "client stopped reading during catch-up; closing it");
                    self.forget(conn);
                    if let Err(error) = self.link.close(conn) {
                        tracing::debug!(%conn, %error, "close failed");
                    }
                    return;
                }
                let wait = (b.wait * 2).min(MAX_CATCH_UP_TICK);
                Blocked {
                    queued,
                    retry_at: now + wait,
                    wait,
                    ..b
                }
            }
            // Newly blocked, or the queue shrank since the last check: progress.
            _ => Blocked {
                queued,
                since: now,
                retry_at: now + CATCH_UP_TICK,
                wait: CATCH_UP_TICK,
            },
        };
        sub.blocked = Some(blocked);
    }

    /// The log catch-up is done: send every live screen, then the ack, and go fully live.
    fn complete(&mut self, conn: ConnId) {
        let snapshots: Vec<Vec<u8>> = self
            .screens
            .iter()
            .map(|(session, screen)| {
                notification_line(&HostNotification::PtyOutput(PtyOutput {
                    session: session.clone(),
                    bytes: screen.snapshot_ansi(),
                }))
            })
            .collect();
        for line in &snapshots {
            self.send(conn, line);
        }
        let Some(sub) = self.conns.get_mut(&conn).and_then(|c| c.sub.as_mut()) else {
            return;
        };
        sub.complete = true;
        let ack = sub.ack.clone();
        self.respond(
            conn,
            &HostResponse {
                id: ack,
                result: Ok(HostResult::Ack),
            },
        );
    }

    fn reply_to(&mut self, conn: ConnId, id: RequestId) -> ReplyTo {
        self.next_reply += 1;
        let reply = ReplyTo::new(self.next_reply);
        self.replies.insert(reply, (conn, id));
        reply
    }

    fn reply(&mut self, to: ReplyTo, result: Result<HostResult, WireError>) {
        // A client that went away before its answer is simply not answered.
        if let Some((conn, id)) = self.replies.remove(&to) {
            self.respond(conn, &HostResponse { id, result });
        }
    }

    fn respond(&mut self, conn: ConnId, response: &HostResponse) {
        self.send(conn, &without_newline(wire::encode_response(response)));
    }

    fn send(&mut self, conn: ConnId, line: &[u8]) {
        if let Err(error) = self.link.send(conn, line) {
            // A conn the link no longer knows has disconnected; its `Disconnected` is on its way.
            tracing::debug!(%conn, %error, "send failed");
        }
    }

    fn forget(&mut self, conn: ConnId) {
        self.conns.remove(&conn);
        self.replies.retain(|_, (c, _)| *c != conn);
    }

    // ---------- shutdown ----------

    fn start_shutdown(&mut self) {
        if self.shutdown_by.is_some() {
            return;
        }
        self.shutdown_by = Some(self.env.instant() + SHUTDOWN_GRACE);
        let live: Vec<SessionId> = self.screens.keys().cloned().collect();
        for session in live {
            if let Err(error) = self.pty.kill(&session) {
                tracing::warn!(%session, %error, "kill failed during shutdown");
            }
        }
        self.check_shutdown_done();
    }

    fn check_shutdown_done(&mut self) {
        if self.shutdown_by.is_some() && self.screens.is_empty() {
            self.finish();
        }
    }

    fn finish(&mut self) {
        let conns: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in conns {
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
        self.conns.clear();
        self.replies.clear();
        self.finished = true;
    }
}

/// A session's screen panicked and was replaced with a blank one ([`screen_guard`]).
fn log_screen_panic(session: &SessionId, result: Result<(), String>) {
    if let Err(panic) = result {
        tracing::error!(%session, %panic, "screen panicked; replaced it with a blank one");
    }
}

/// `cwd` as a directory a session can start in: absolute, since a client can't know where the host started, and an
/// existing directory.
fn existing_dir(cwd: &str) -> Result<PathBuf, WireError> {
    let path = PathBuf::from(cwd);
    if !path.is_absolute() {
        return Err(WireError::InvalidParams(format!(
            "cwd `{cwd}` is not an absolute path"
        )));
    }
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Ok(path),
        Ok(_) => Err(WireError::InvalidParams(format!(
            "cwd `{cwd}` is not a directory"
        ))),
        Err(e) => Err(WireError::InvalidParams(format!("cwd `{cwd}`: {e}"))),
    }
}

fn log_entry_line(envelope: Envelope) -> Vec<u8> {
    notification_line(&HostNotification::LogEntry(Box::new(LogEntry { envelope })))
}

fn notification_line(note: &HostNotification) -> Vec<u8> {
    without_newline(wire::encode_host_notification(note))
}

/// Encoders end each message with `\n`; a `Link` takes lines without it.
fn without_newline(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    line
}
