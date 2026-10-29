//! The host runtime loop (decision 0011): one thread draining one unbounded channel of [`RuntimeEvent`]s. For each
//! event it calls [`Host::handle`] and executes the commands in order through the ports, following the runtime
//! contract documented on [`Host`].
//!
//! Besides what core decides, the runtime owns three things that aren't domain state:
//! - **Screens.** One `shepherd_term::Screen` per live PTY, fed every output chunk. Its query answers are written
//!   back to the PTY, so agents get them with no client attached (decision 0007). A synchronized update's hold is
//!   serviced as a deadline.
//! - **Subscriptions.** Per connection. A subscribe is handled in one loop iteration: the log catch-up
//!   (`EventLog::read`), then a `snapshot_ansi()` of every live screen, then the ack, and from then on every new
//!   envelope and output chunk. Appends only happen on this thread, so there is no gap and no duplicate.
//! - **Replies.** Each request turned into a [`HostEvent`] gets a [`ReplyTo`] that maps back to its connection and
//!   JSON-RPC id.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use shepherd_core::acpp::{Envelope, FrameDraft, RequestId, Seq, Stamp};
use shepherd_core::config::Config;
use shepherd_core::host::{Host, HostCommand, HostEvent, NewSessionSpec, ReplyTo};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    ConnId, EventLog, Link, LinkEvent, LinkEventSink, LogError, LogId, Pty, PtyEvent, PtyEventSink,
    PtySize, SpawnSpec,
};
use shepherd_core::wire::{
    self, ClientMessage, ClientNotification, ClientRequest, HostNotification, HostResponse,
    HostResult, LogEntry, PtyOutput, SubscribeFrom, WireError,
};
use shepherd_term::Screen;

use super::env::Env;

/// The machine id every entry is stamped with until machines get names (T29).
pub const LOCAL_MACHINE: &str = "local";
/// The size a new session's PTY starts at, until a client resizes it.
pub const INITIAL_SIZE: PtySize = PtySize { cols: 80, rows: 24 };
/// How long shutdown waits for killed PTYs to report their exit before giving up on them.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
pub struct RuntimeTx(Sender<RuntimeEvent>);

impl RuntimeTx {
    /// Queues `event`. Never blocks. Dropped silently once the loop has stopped.
    pub fn send(&self, event: RuntimeEvent) {
        let _ = self.0.send(event);
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
    (RuntimeTx(tx), RuntimeRx(rx))
}

/// What the runtime needs besides its ports.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub config: Config,
    /// Where new sessions start.
    pub home: PathBuf,
}

#[derive(Debug, Default)]
struct ConnState {
    subscribed: bool,
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
        Ok(runtime)
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn log(&self) -> &E {
        &self.log
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
            .chain(self.shutdown_by)
            .min()
    }

    /// Runs until shutdown completes, sleeping in `recv_timeout` until the next event or deadline.
    pub fn run(&mut self) {
        while !self.finished {
            let next = match self.next_deadline() {
                None => self.rx.0.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(due) => {
                    let wait = due.saturating_duration_since(self.env.instant());
                    self.rx.0.recv_timeout(wait)
                }
            };
            match next {
                Ok(event) => self.handle(event),
                Err(RecvTimeoutError::Timeout) => self.fire_due(),
                // The runtime holds a sender, so this can't happen; stop rather than spin.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Handles every event already queued and every deadline already due, until there is none. For tests, where
    /// the fakes queue their events synchronously.
    pub fn run_until_idle(&mut self) {
        loop {
            self.fire_due();
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
                        tracing::error!(%error, "append failed; skipping the rest of the batch");
                        for rest in commands.by_ref() {
                            if let HostCommand::Reply { to, .. } = rest {
                                let error = WireError::Internal(format!("append failed: {error}"));
                                self.reply(to, Err(error));
                            }
                        }
                        return;
                    }
                }
                HostCommand::SpawnPty { session, spec } => {
                    let size = spec.size;
                    match self.pty.spawn(session.clone(), spec) {
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

    /// Seals and appends `draft`, applies the envelope, and fans it out to subscribers.
    fn append(&mut self, draft: FrameDraft) -> Result<(), LogError> {
        let stamp = Stamp {
            id: self.env.new_entry_id(),
            ts: self.env.now(),
            machine: self.machine.clone(),
        };
        let envelope = self.log.append(LogId::of(&draft), draft, stamp)?;
        self.host.apply(&envelope);
        let line = log_entry_line(envelope);
        self.broadcast(&line);
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
            screen.feed(&bytes);
            self.after_feed(&session);
        }
        let line = notification_line(&HostNotification::PtyOutput(PtyOutput { session, bytes }));
        self.broadcast(&line);
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

    /// Ends every synchronized update whose deadline has passed, and finishes a shutdown that ran out of time.
    fn fire_due(&mut self) {
        let now = self.env.instant();
        let due: Vec<(SessionId, u64)> = self
            .deadlines
            .iter()
            .filter(|(_, d)| d.due <= now)
            .map(|(s, d)| (s.clone(), d.hold))
            .collect();
        for (session, hold) in due {
            self.deadlines.remove(&session);
            if let Some(screen) = self.screens.get_mut(&session) {
                screen.expire_sync(hold);
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
                let result = wire::negotiate(&hello, &self.machine).map(HostResult::Hello);
                self.respond(conn, &respond_now(result));
            }
            ClientRequest::Subscribe(subscribe) => {
                let result = self
                    .subscribe(conn, &subscribe.from)
                    .map(|()| HostResult::Ack);
                self.respond(conn, &respond_now(result));
            }
            ClientRequest::HookFrame(_) => {
                let error = WireError::MethodNotFound(format!(
                    "{} is not available on this host yet",
                    wire::Method::HookFrame.as_str()
                ));
                self.respond(conn, &respond_now(Err(error)));
            }
            ClientRequest::SessionNew(_) => {
                let reply = self.reply_to(conn, id);
                let event = HostEvent::NewSession {
                    reply,
                    id: self.env.new_session_id(),
                    at: self.env.now(),
                    spec: NewSessionSpec {
                        name: None,
                        spawn: self.shell_spawn(),
                    },
                };
                self.dispatch(event);
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

    /// What a new M1 session runs: the `shell` profile, in the home directory, at [`INITIAL_SIZE`].
    fn shell_spawn(&self) -> SpawnSpec {
        let (command, args, env) = match self.settings.config.profile("shell") {
            Some(p) => (
                p.command.clone(),
                p.args.clone(),
                p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            ),
            // `Config` always has the built-in `shell` profile; this only keeps the lookup total.
            None => ("sh".to_owned(), Vec::new(), Vec::new()),
        };
        SpawnSpec {
            command,
            args,
            cwd: self.settings.home.clone(),
            env,
            size: INITIAL_SIZE,
        }
    }

    /// The log catch-up and screen snapshots for a new subscription, sent before it goes live. Reads everything
    /// first, so a failed read sends nothing.
    fn subscribe(&mut self, conn: ConnId, from: &SubscribeFrom) -> Result<(), WireError> {
        let internal = |e: LogError| WireError::Internal(format!("reading the log: {e}"));
        let mut lines = Vec::new();
        for id in self.log.logs().map_err(internal)? {
            let after = match &id {
                LogId::Host => from.host.unwrap_or(Seq::ZERO),
                LogId::Session(s) => from.sessions.get(s).copied().unwrap_or(Seq::ZERO),
            };
            for envelope in self.log.read(id, after).map_err(internal)? {
                lines.push(log_entry_line(envelope));
            }
        }
        for (session, screen) in &self.screens {
            lines.push(notification_line(&HostNotification::PtyOutput(PtyOutput {
                session: session.clone(),
                bytes: screen.snapshot_ansi(),
            })));
        }
        for line in &lines {
            self.send(conn, line);
        }
        self.conns.entry(conn).or_default().subscribed = true;
        Ok(())
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

    /// Sends `line` to every subscribed conn.
    fn broadcast(&mut self, line: &[u8]) {
        let subscribed: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.subscribed)
            .map(|(&conn, _)| conn)
            .collect();
        for conn in subscribed {
            self.send(conn, line);
        }
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
