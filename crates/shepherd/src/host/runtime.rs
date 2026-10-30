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
//! - **Worktrees calls.** The `Worktrees` port runs on a [`Worker`] thread, since its calls may wait on the network.
//!   A command that needs it (a workspace probe) is submitted there and the loop carries on; the result comes back as
//!   [`RuntimeEvent::Worktrees`] and goes to core, which decides whether it still matters. Closing a session inspects
//!   and removes its worktree the same way. A new session's worktree create runs on a thread of its own instead
//!   ([`Worker::run_alone`]), since its checkout has no time cap: creates for different sessions run at the same time,
//!   and none holds up the worker. Closing a session whose create is running cancels it (`Worktrees::cancel_create`,
//!   which never waits on git), and the create's answer comes back as usual; shutting the host down cancels every
//!   running create. The `Commands` port (a worktree's
//!   post-create and pre-remove hooks) runs on a worker of its own the same way, its results coming back as
//!   [`RuntimeEvent::Commands`], so a long hook never holds up git calls or the loop.
//! - **Run files.** A new session's launch (`launch::plan`, from its profile) may come with files, e.g. Claude
//!   Code's generated settings. They are written into the session's run directory just before its PTY is spawned
//!   (a failure to write them fails the spawn), and the directory is removed when its PTY's exit or its
//!   `session/closed` is appended. A session waiting for its worktree or its hooks keeps its files until the last
//!   result comes back (they are written if it spawns, dropped if not). At startup, when no PTY is running, every leftover run directory is swept. A
//!   resumed session's launch (`launch::plan_resume`) writes them again the same way.
//! - **Updating in place** (decision 0018). `_shepherd/host/upgrade` is decided by core
//!   ([`shepherd_core::host::upgrade::decide`]); its preflight runs on a thread of its own and comes back as
//!   [`RuntimeEvent::Preflighted`]. Once accepted, the runtime exports its PTYs (their pumps paused, everything read so
//!   far queued), handles what is queued (screens fed, hook frames appended, other requests dropped), and hands a
//!   [`SwapPlan`] (each session's token, size and `snapshot_ansi`, and the translators) out of [`HostRuntime::run`].
//!   The caller stops accepting, closes every conn ([`HostRuntime::close_conns`]) and re-execs; if that fails, [`HostRuntime::swap_failed`] resumes the PTYs and
//!   later upgrades are answered with why. The new image starts with [`HostRuntime::adopt`]: adopted sessions get no
//!   exit and keep their run directory, their screens are fed their snapshots, the translators are restored, and each
//!   is nudged to redraw.
//! - **Quiet watch.** A [`QuietWatch`] (the quiet-output idle fallback) sees every entry appended, and the logs on
//!   start. Output of, and client input to, the sessions it watches are fed to it stamped with `Env::now`; its
//!   deadline is serviced like the others: when it is due, a tick stamped with `Env::now` returns the idles to append.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use shepherd_core::acpp::{
    Envelope, Frame, FrameDraft, RequestId, Seq, ShepherdFrame, SourceKind, Stamp, Timestamp,
};
use shepherd_core::agents::{CarriedTranslators, Translators};
use shepherd_core::config::{AgentKind, Config, WorkspaceHooks};
use shepherd_core::host::launch::{self, LaunchContext, RunFile};
use shepherd_core::host::quiet::{QuietEvent, QuietWatch};
use shepherd_core::host::upgrade::{Preflight, UpgradeFacts, UpgradeStep, Work, decide};
use shepherd_core::host::{
    Host, HostCommand, HostEvent, Inspection, NewSessionSpec, ReplyTo, ResumeSpec, WorkspaceAsk,
    expand_home,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    CommandError, Commands, ConnId, EventLog, Link, LinkEvent, LinkEventSink, LogError, LogId,
    MAX_QUEUED, PtyError, PtyEvent, PtyEventSink, PtyHandoff, PtySize, RepoRef, WorktreeError,
    WorktreeName, WorktreePath, Worktrees,
};
use shepherd_core::wire::{
    self, BuildId, ClientMessage, ClientNotification, ClientRequest, ExeDigest, HookFrame,
    HostNotification, HostResponse, HostResult, HostUpgrade, LogEntry, PtyOutput, SessionNew,
    SubscribeFrom, UpgradeAnswer, WireError,
};
use shepherd_term::Screen;

use super::env::Env;
use super::run_files;
use super::worker::Worker;
use crate::build::InstalledExe;
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
    /// The result of a `Worktrees` call that ran on the worker thread, for core.
    Worktrees(HostEvent),
    /// The result of a `Commands` call (a hook) that ran on its worker thread, for core.
    Commands(HostEvent),
    /// SIGTERM or SIGINT: kill the PTYs, record their exits, close the links, stop.
    Shutdown,
    /// How the upgrade preflight (`<shepherd> handoff-check`) ended.
    Preflighted(Preflight),
}

/// Runs the upgrade preflight: `<exe> handoff-check`, bounded. The runtime calls it on a thread of its own.
#[derive(Clone)]
pub struct PreflightRunner(Arc<dyn Fn(&Path) -> Preflight + Send + Sync>);

impl PreflightRunner {
    pub fn new(run: impl Fn(&Path) -> Preflight + Send + Sync + 'static) -> Self {
        Self(Arc::new(run))
    }

    pub fn run(&self, exe: &Path) -> Preflight {
        (self.0)(exe)
    }
}

impl std::fmt::Debug for PreflightRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreflightRunner")
    }
}

/// One session as a host hands it to its next image.
#[derive(Debug)]
pub struct CarriedSession<T> {
    pub id: SessionId,
    /// Its screen's size, which its terminal has too.
    pub cols: u16,
    pub rows: u16,
    /// Its screen, as `Screen::snapshot_ansi` replays it.
    pub snapshot: Vec<u8>,
    /// What the `PtyHandoff` port needs to adopt it.
    pub token: T,
}

/// What an accepted upgrade carries across the exec besides the log, its lock and the listener.
#[derive(Debug)]
pub struct SwapPlan<T> {
    pub sessions: Vec<CarriedSession<T>>,
    pub translators: CarriedTranslators,
}

/// A session the new image adopted: its PTY is in the `Pty` port already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedSession {
    pub id: SessionId,
    pub cols: u16,
    pub rows: u16,
    pub snapshot: Vec<u8>,
}

/// What [`HostRuntime::adopt`] starts from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Adoption {
    pub sessions: Vec<AdoptedSession>,
    pub translators: CarriedTranslators,
}

/// Adopts every carried session into `pty`, before the logs are folded, so their output keeps draining. A session
/// the port refuses is left out (the port has killed it), and so is recorded as exited at start.
pub fn adopt_sessions<P: PtyHandoff>(pty: &mut P, plan: SwapPlan<P::Token>) -> Adoption {
    let mut sessions = Vec::new();
    for carried in plan.sessions {
        let CarriedSession {
            id,
            cols,
            rows,
            snapshot,
            token,
        } = carried;
        match pty.adopt(id.clone(), token) {
            Ok(()) => sessions.push(AdoptedSession {
                id,
                cols,
                rows,
                snapshot,
            }),
            Err(error) => {
                tracing::warn!(session = %id, %error, "cannot adopt session; it was killed")
            }
        }
    }
    Adoption {
        sessions,
        translators: plan.translators,
    }
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

#[cfg(test)]
impl RuntimeRx {
    /// The next event, waiting up to `limit` (a liveness bound).
    pub(crate) fn recv_timeout(&self, limit: Duration) -> Option<RuntimeEvent> {
        self.0.recv_timeout(limit).ok()
    }
}

/// A new channel for one runtime. Build the port sinks from the sender before building the ports.
pub fn runtime_channel() -> (RuntimeTx, RuntimeRx) {
    let (tx, rx) = mpsc::channel();
    let tx = RuntimeTx {
        tx,
        shutdown: Arc::default(),
    };
    (tx, RuntimeRx(rx))
}

/// Why [`HostRuntime::start`] failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("cannot start a port's worker thread: {0}")]
    Worker(std::io::Error),
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
    /// The build this host runs, read from `shepherd` at startup. `None` if it couldn't be read.
    pub build: Option<BuildId>,
    /// What is installed at `shepherd` now, for the hello: what a restart would run.
    pub installed: InstalledExe,
    /// Runs `<shepherd> handoff-check` before an upgrade.
    pub preflight: PreflightRunner,
}

/// An upgrade request being decided.
#[derive(Debug)]
struct PendingUpgrade {
    reply: ReplyTo,
    force: bool,
    preflight: Option<Preflight>,
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
pub struct HostRuntime<P: PtyHandoff, W, C, L, E, V> {
    host: Host,
    pty: P,
    worktrees: Worker<W>,
    /// `Worktrees` calls submitted and not yet answered.
    worktree_calls: usize,
    commands: Worker<C>,
    /// `Commands` calls (hooks) submitted and not yet answered.
    hook_calls: usize,
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
    /// Sessions whose worktree is being created or whose hooks are running, with the run files their spawn will write.
    awaiting_worktree: HashMap<SessionId, Vec<RunFile>>,
    /// Sessions whose worktree create is running, with what it was called with, for `Worktrees::cancel_create`.
    creating: HashMap<SessionId, (RepoRef, WorktreeName)>,
    /// Set once shutdown starts: when it gives up waiting for PTY exits.
    shutdown_by: Option<Instant>,
    finished: bool,
    quiet: QuietWatch,
    /// When the quiet watch's deadline is due on the monotonic clock.
    quiet_due: Option<Instant>,
    upgrade: Option<PendingUpgrade>,
    /// The preflight is running.
    preflighting: bool,
    /// An accepted upgrade has exported the PTYs: only hook frames are still taken.
    swapping: bool,
    /// A swap ready to exec, for [`HostRuntime::run`] to hand out.
    swap: Option<SwapPlan<P::Token>>,
    /// Why the last accepted swap failed, and what was installed then.
    swap_failed: Option<(Option<ExeDigest>, String)>,
}

impl<
    P: PtyHandoff,
    W: Worktrees + Send + Sync + 'static,
    C: Commands + Send + Sync + 'static,
    L: Link,
    E: EventLog,
    V: Env,
> HostRuntime<P, W, C, L, E, V>
{
    /// Rebuilds the host state by folding every log, then records an exit for every session that was still
    /// running: PTYs never survive a host restart. `tx` and `rx` are the channel the ports' sinks feed. `worktrees`
    /// and `commands` each move to a worker thread of their own.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        pty: P,
        worktrees: W,
        commands: C,
        link: L,
        log: E,
        env: V,
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
    ) -> Result<Self, StartError> {
        let ports = (pty, worktrees, commands, link, log, env);
        Self::start_with(ports, settings, tx, rx, Adoption::default())
    }

    /// Starts the new image after a swap, like [`HostRuntime::start`] but with `adoption`'s sessions running in `pty`
    /// (see [`adopt_sessions`]): they get no exit and keep their run directories, their screens start from their
    /// snapshots, the carried translators are restored (one that doesn't parse is dropped), and each session is
    /// nudged to redraw.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        pty: P,
        worktrees: W,
        commands: C,
        link: L,
        log: E,
        env: V,
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
        adoption: Adoption,
    ) -> Result<Self, StartError> {
        let ports = (pty, worktrees, commands, link, log, env);
        Self::start_with(ports, settings, tx, rx, adoption)
    }

    fn start_with(
        (pty, worktrees, commands, link, log, env): (P, W, C, L, E, V),
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
        adoption: Adoption,
    ) -> Result<Self, StartError> {
        let mut host = Host::new();
        let mut quiet = QuietWatch::new(&settings.config);
        for id in log.logs()? {
            for envelope in log.read(id, Seq::ZERO)? {
                host.apply(&envelope);
                quiet.apply(&envelope);
            }
        }
        let worktrees = Worker::spawn("worktrees", worktrees, tx.clone(), RuntimeEvent::Worktrees)
            .map_err(StartError::Worker)?;
        let commands = Worker::spawn("hooks", commands, tx.clone(), RuntimeEvent::Commands)
            .map_err(StartError::Worker)?;
        let mut runtime = Self {
            host,
            pty,
            worktrees,
            worktree_calls: 0,
            commands,
            hook_calls: 0,
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
            awaiting_worktree: HashMap::new(),
            creating: HashMap::new(),
            shutdown_by: None,
            finished: false,
            quiet,
            quiet_due: None,
            upgrade: None,
            preflighting: false,
            swapping: false,
            swap: None,
            swap_failed: None,
        };
        let adopted: BTreeSet<SessionId> = adoption.sessions.iter().map(|s| s.id.clone()).collect();
        let orphans: Vec<SessionId> = runtime
            .host
            .sessions()
            .into_iter()
            .filter(|s| !s.exited && !adopted.contains(&s.id))
            .map(|s| s.id)
            .collect();
        for session in orphans {
            runtime.dispatch(HostEvent::PtyExited {
                session,
                code: None,
            });
        }
        // A session that was closing lost its worktree's inspection, hooks or removal with the old host.
        runtime.dispatch(HostEvent::Restarted);
        // Every other run directory is stale: the log lock is held and nothing else has been spawned yet.
        run_files::sweep_except(&runtime.settings.run_root, &adopted);
        runtime.restore(adoption);
        Ok(runtime)
    }

    /// Restores what an earlier image carried: screens, translators, and a redraw of each adopted session.
    fn restore(&mut self, adoption: Adoption) {
        let (translators, dropped) = Translators::restore(adoption.translators);
        for session in dropped {
            tracing::warn!(%session, "dropped a carried hook translator this build can't read");
        }
        self.translators = translators;
        for adopted in adoption.sessions {
            let AdoptedSession {
                id,
                cols,
                rows,
                snapshot,
            } = adopted;
            let mut screen = Screen::new(cols, rows);
            let fed = screen_guard::update(&mut screen, |s| s.feed(&snapshot));
            log_screen_panic(&id, fed);
            // Queries in a replayed screen were answered by the image that saw them first.
            screen.take_replies();
            self.screens.insert(id.clone(), screen);
            self.after_feed(&id);
            self.nudge(&id, PtySize { cols, rows });
        }
    }

    /// Makes `session`'s program redraw: resizing to the same size doesn't signal it, so it is resized one column
    /// narrower and back.
    fn nudge(&mut self, session: &SessionId, size: PtySize) {
        let other = PtySize {
            cols: if size.cols > 1 {
                size.cols - 1
            } else {
                size.cols + 1
            },
            ..size
        };
        for size in [other, size] {
            if let Err(error) = self.pty.resize(session, size) {
                tracing::warn!(%session, %error, "cannot nudge an adopted session to redraw");
                return;
            }
        }
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

    /// How many sessions have run files waiting for their spawn, those waiting for their worktree included. Zero
    /// when no session is waiting.
    pub fn pending_run_files(&self) -> usize {
        self.pending_run_files.len() + self.awaiting_worktree.len()
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
            .chain(self.quiet_due)
            .min()
    }

    /// Runs until shutdown completes (`None`) or an accepted upgrade is ready to exec (its plan), sleeping in
    /// `recv_timeout` until the next event or deadline. A shutdown request and due deadlines are serviced on every
    /// iteration, so a busy channel can't starve them.
    pub fn run(&mut self) -> Option<SwapPlan<P::Token>> {
        loop {
            self.service();
            if self.finished {
                return None;
            }
            if let Some(plan) = self.swap.take() {
                return Some(plan);
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
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The swap an accepted upgrade has made ready, if any. [`HostRuntime::run`] returns it; for tests stepping the
    /// loop themselves.
    pub fn take_swap(&mut self) -> Option<SwapPlan<P::Token>> {
        self.swap.take()
    }

    /// Closes every conn, for a swap: after the listener is handed over, so a client reconnecting at once waits in the
    /// backlog for the new image instead of being accepted by this one and cut off by the exec. What was sent to each,
    /// the upgrade's answer included, is flushed first.
    pub fn close_conns(&mut self) {
        let conns: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in conns {
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
        self.conns.clear();
        self.replies.clear();
    }

    /// The exec of a swap failed with `why`: the PTYs are resumed and served as before, and the next upgrade
    /// request is answered with why, until another build is installed. The conns closed for the swap stay closed;
    /// clients reconnect.
    pub fn swap_failed(&mut self, why: String) {
        tracing::error!(%why, "updating in place failed; carrying on");
        self.pty.resume();
        self.swapping = false;
        self.swap = None;
        let installed = self.settings.installed.digest();
        self.swap_failed = Some((installed, why));
    }

    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    pub fn settings(&self) -> &RuntimeSettings {
        &self.settings
    }

    /// Handles every event already queued and every deadline already due, until there is none. For tests, where
    /// the fakes queue their events synchronously. Doesn't wait for `Worktrees` calls on the worker thread.
    pub fn run_until_idle(&mut self) {
        self.run_events(usize::MAX);
    }

    /// Like [`run_until_idle`](Self::run_until_idle), but handles at most `max` events. Returns how many it
    /// handled. For tests whose fakes never go idle, such as a flooding PTY.
    pub fn run_events(&mut self, max: usize) -> usize {
        let mut handled = 0;
        while handled < max {
            self.service();
            match self.rx.0.try_recv() {
                Ok(event) => self.handle(event),
                Err(_) => break,
            }
            handled += 1;
        }
        handled
    }

    /// [`HostRuntime::run_until_idle`], and then, while a `Worktrees` or `Commands` call is still unanswered, waits for
    /// the next event and goes on. For tests. `limit` bounds each wait, for liveness; false if one ran out.
    pub fn run_until_settled(&mut self, limit: Duration) -> bool {
        loop {
            self.run_until_idle();
            if self.worktree_calls + self.hook_calls == 0 && !self.preflighting || self.finished {
                return true;
            }
            match self.rx.0.recv_timeout(limit) {
                Ok(event) => self.handle(event),
                Err(_) => return false,
            }
        }
    }

    /// Waits up to `limit` (a liveness bound) for the next event and handles it, then everything already queued, as
    /// [`run_until_idle`](Self::run_until_idle) does. For tests waiting on one port call while holding another. False if
    /// nothing arrived in time.
    pub fn run_next(&mut self, limit: Duration) -> bool {
        match self.rx.0.recv_timeout(limit) {
            Ok(event) => self.handle(event),
            Err(_) => return false,
        }
        self.run_until_idle();
        true
    }

    /// Handles one event.
    pub fn handle(&mut self, event: RuntimeEvent) {
        if self.finished {
            return;
        }
        match event {
            // Output while shutting down is dropped unread, so a flooding PTY's backlog drains fast and the exits
            // queued behind it are reached. All output is acknowledged once handled, which is what bounds a
            // flooding session's share of the channel (the `Pty` port's credit).
            RuntimeEvent::Pty(PtyEvent::Output { session, bytes })
                if self.shutdown_by.is_some() =>
            {
                self.ack(&session, bytes.len());
            }
            RuntimeEvent::Pty(PtyEvent::Output { session, bytes }) => {
                let n = bytes.len();
                self.pty_output(session.clone(), bytes);
                self.ack(&session, n);
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
            RuntimeEvent::Worktrees(event) => {
                self.worktree_calls = self.worktree_calls.saturating_sub(1);
                match &event {
                    HostEvent::WorktreeCreated { session, .. } => {
                        let session = session.clone();
                        self.worktree_created(session, event);
                    }
                    _ => self.dispatch(event),
                }
            }
            RuntimeEvent::Commands(event) => {
                self.hook_calls = self.hook_calls.saturating_sub(1);
                match &event {
                    HostEvent::HookFinished { session, .. } => {
                        let session = session.clone();
                        self.worktree_created(session, event);
                    }
                    _ => self.dispatch(event),
                }
            }
            RuntimeEvent::Shutdown => self.start_shutdown(),
            RuntimeEvent::Preflighted(preflight) => {
                self.preflighting = false;
                if let Some(pending) = &mut self.upgrade {
                    pending.preflight = Some(preflight);
                    self.advance_upgrade();
                }
            }
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

    /// Executes `commands` in order. Events they produce (a failed spawn, a translated hook) are pushed to `follow_up`.
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
                    // Core decides what follows from the frames, e.g. naming the session after its first prompt.
                    follow_up.push_back(HostEvent::HookTranslated { session });
                }
                HostCommand::SpawnPty { session, spec } => {
                    let size = spec.size;
                    // Every spawn's cwd is checked: the PTY would silently start in the home directory instead.
                    let spawned = existing_dir(&spec.cwd)
                        .map_err(|e| e.to_string())
                        .and_then(|_| self.write_run_files(&session))
                        .and_then(|()| {
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
                HostCommand::WritePty {
                    session,
                    bytes,
                    kind,
                } => {
                    self.write_pty(&session, &bytes);
                    // A client's input: the quiet watch decides whether it answers a permission dialog.
                    if self.quiet.watches(&session) {
                        let at = self.env.now();
                        self.quiet.handle(QuietEvent::Input {
                            session,
                            at: at.clone(),
                            kind,
                            bytes,
                        });
                        self.schedule_quiet(&at);
                    }
                }
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
                HostCommand::ProbeWorkspace { ask, path } => {
                    let panicked = HostEvent::WorkspaceProbed {
                        ask,
                        repo: Err(format!(
                            "{}: probing it failed unexpectedly",
                            path.display()
                        )),
                    };
                    let call = move |worktrees: &W| HostEvent::WorkspaceProbed {
                        ask,
                        repo: probe_workspace(worktrees, &path),
                    };
                    match self.worktrees.submit(call, panicked) {
                        Ok(()) => self.worktree_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the worktrees thread is gone");
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::CreateWorktree {
                    session,
                    repo,
                    name,
                    base,
                    spawn,
                    hooks,
                } => {
                    // Its run files wait with it, for the spawn the result may bring.
                    let files = self.pending_run_files.remove(&session).unwrap_or_default();
                    self.awaiting_worktree.insert(session.clone(), files);
                    let panicked = HostEvent::WorktreeCreated {
                        session: session.clone(),
                        spawn: spawn.clone(),
                        hooks: hooks.clone(),
                        result: Err(WorktreeError::Other(format!(
                            "creating worktree {name} failed unexpectedly"
                        ))),
                    };
                    self.creating
                        .insert(session.clone(), (repo.clone(), name.clone()));
                    let thread = format!("worktree-create-{name}");
                    let call = move |worktrees: &W| HostEvent::WorktreeCreated {
                        result: worktrees.create(&repo, &name, base),
                        session,
                        spawn,
                        hooks,
                    };
                    // On a thread of its own: a checkout has no time cap, and must not hold up other sessions' creates
                    // or the worker's other calls.
                    match self.worktrees.run_alone(&thread, call, panicked) {
                        Ok(()) => self.worktree_calls += 1,
                        Err(unanswered) => {
                            // Nothing will spawn: its run files go with the failure.
                            if let HostEvent::WorktreeCreated { session, .. } = &*unanswered {
                                self.awaiting_worktree.remove(session);
                                self.creating.remove(session);
                            }
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::CancelWorktree { session } => {
                    // Never waits on git: the create's own thread waits for its git to be gone and cleans up, then
                    // answers as usual.
                    if let Some((repo, name)) = self.creating.get(&session) {
                        self.worktrees.port().cancel_create(repo, name);
                    }
                }
                HostCommand::RunHook {
                    session,
                    command,
                    rest,
                    spawn,
                } => {
                    // Its run files wait with it, for the spawn after the hooks.
                    let files = self.pending_run_files.remove(&session).unwrap_or_default();
                    self.awaiting_worktree.insert(session.clone(), files);
                    let panicked = HostEvent::HookFinished {
                        session: session.clone(),
                        command: command.clone(),
                        rest: rest.clone(),
                        spawn: spawn.clone(),
                        result: Err(CommandError::Failed(
                            "running it failed unexpectedly".into(),
                        )),
                    };
                    let call = move |commands: &C| HostEvent::HookFinished {
                        result: commands.run(&command),
                        session,
                        command,
                        rest,
                        spawn,
                    };
                    match self.commands.submit(call, panicked) {
                        Ok(()) => self.hook_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the hooks thread is gone");
                            // Core still spawns after a hook that couldn't run: its run files are ready for it.
                            if let HostEvent::HookFinished { session, .. } = &*unanswered
                                && let Some(files) = self.awaiting_worktree.remove(session)
                            {
                                self.pending_run_files.insert(session.clone(), files);
                            }
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::InspectWorktree {
                    session,
                    repo,
                    path,
                    purpose,
                } => {
                    let panicked = HostEvent::WorktreeInspected {
                        session: session.clone(),
                        path: path.clone(),
                        purpose: purpose.clone(),
                        result: Err(WorktreeError::Other(format!(
                            "inspecting worktree {path} failed unexpectedly"
                        ))),
                    };
                    let call = move |worktrees: &W| HostEvent::WorktreeInspected {
                        result: inspect_worktree(worktrees, &repo, &path),
                        session,
                        path,
                        purpose,
                    };
                    match self.worktrees.submit(call, panicked) {
                        Ok(()) => self.worktree_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the worktrees thread is gone");
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::RunPreRemove {
                    session,
                    command,
                    rest,
                    path,
                    force,
                } => {
                    let panicked = HostEvent::PreRemoveFinished {
                        session: session.clone(),
                        command: command.clone(),
                        rest: rest.clone(),
                        path: path.clone(),
                        force,
                        result: Err(CommandError::Failed(
                            "running it failed unexpectedly".into(),
                        )),
                    };
                    let call = move |commands: &C| HostEvent::PreRemoveFinished {
                        result: commands.run(&command),
                        session,
                        command,
                        rest,
                        path,
                        force,
                    };
                    match self.commands.submit(call, panicked) {
                        Ok(()) => self.hook_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the hooks thread is gone");
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::RemoveWorktree {
                    session,
                    path,
                    force,
                } => {
                    let panicked = HostEvent::WorktreeRemoved {
                        session: session.clone(),
                        path: path.clone(),
                        result: Err(WorktreeError::Other(format!(
                            "removing worktree {path} failed unexpectedly"
                        ))),
                        exists: true,
                    };
                    let call = move |worktrees: &W| {
                        let result = worktrees.remove(&path, force);
                        HostEvent::WorktreeRemoved {
                            exists: anything_at(&path),
                            result,
                            session,
                            path,
                        }
                    };
                    match self.worktrees.submit(call, panicked) {
                        Ok(()) => self.worktree_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the worktrees thread is gone");
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
            }
        }
    }

    /// Hands a session's worktree or hook result (`event`) to core, with the run files that waited for it ready for its
    /// spawn. If core runs another hook instead, they go on waiting with it.
    fn worktree_created(&mut self, session: SessionId, event: HostEvent) {
        if matches!(event, HostEvent::WorktreeCreated { .. }) {
            self.creating.remove(&session);
        }
        let files = self.awaiting_worktree.remove(&session).unwrap_or_default();
        if !files.is_empty() {
            self.pending_run_files.insert(session.clone(), files);
        }
        let closed = self.host.resume_target(&session).is_none();
        if let (true, HostEvent::WorktreeCreated { result: Ok(c), .. }) = (closed, &event) {
            tracing::info!(%session, path = %c.path, "worktree made for a session closed meanwhile; left for cleanup");
        }
        self.dispatch(event);
        // Left over only if nothing was spawned.
        self.pending_run_files.remove(&session);
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
        self.quiet.apply(&envelope);
        self.schedule_quiet(envelope.ts());
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

    /// Returns `bytes` of credit to `session` once its output is processed, so the PTY reads more (the port's flow
    /// control). A session that has exited since is no longer known to the port; that's expected.
    fn ack(&mut self, session: &SessionId, bytes: usize) {
        match self.pty.ack(session, bytes) {
            Ok(()) | Err(PtyError::UnknownSession) => {}
            Err(error) => tracing::warn!(%session, %error, "ack failed"),
        }
    }

    fn pty_output(&mut self, session: SessionId, bytes: Vec<u8>) {
        if self.quiet.watches(&session) {
            let at = self.env.now();
            self.quiet.handle(QuietEvent::Output {
                session: session.clone(),
                at: at.clone(),
            });
            self.schedule_quiet(&at);
        }
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
        if self.quiet_due.is_some_and(|due| due <= now) && self.shutdown_by.is_none() {
            self.tick_quiet();
        }
        if self.shutdown_by.is_some_and(|by| by <= now) && !self.finished {
            tracing::warn!(
                left = self.screens.len(),
                "PTYs didn't exit in time; stopping anyway"
            );
            self.finish();
        }
    }

    /// Appends the idles the quiet watch infers now, and schedules its next deadline.
    fn tick_quiet(&mut self) {
        let at = self.env.now();
        let commands = self.quiet.handle(QuietEvent::Tick { at: at.clone() });
        let mut follow_up = VecDeque::new();
        self.execute(commands, &mut follow_up);
        for event in follow_up {
            self.dispatch(event);
        }
        self.schedule_quiet(&at);
    }

    /// Puts the quiet watch's deadline on the monotonic clock, `at` being the wall-clock time now.
    fn schedule_quiet(&mut self, at: &Timestamp) {
        self.quiet_due = self.quiet.deadline().map(|deadline| {
            let wait =
                Duration::try_from(deadline.duration_since(at.instant())).unwrap_or(Duration::ZERO);
            self.env.instant() + wait
        });
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
            Ok(ClientMessage::Notification(_)) if self.swapping => {}
            Ok(ClientMessage::Notification(note)) => match note {
                ClientNotification::SessionInput(input) => self.dispatch(HostEvent::Input {
                    session: input.session,
                    bytes: input.data,
                    kind: input.kind,
                }),
                ClientNotification::SessionResize(resize) => self.dispatch(HostEvent::Resize {
                    session: resize.session,
                    size: PtySize {
                        cols: resize.cols,
                        rows: resize.rows,
                    },
                }),
            },
            // Once the PTYs are exported only hook frames are taken, so nothing is started that the swap can't carry.
            Ok(ClientMessage::Request(request))
                if self.swapping && !matches!(request.body, ClientRequest::HookFrame(_)) =>
            {
                tracing::debug!(%conn, "dropped a request during the swap");
            }
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
                let installed = self.settings.installed.digest();
                let settings = &self.settings;
                let config = &settings.config;
                let result = wire::negotiate(&hello, &self.machine).map(|ok| {
                    let ok = ok.with_profiles(config).with_notify(config);
                    HostResult::Hello(ok.with_build(settings.build.clone(), installed))
                });
                let accepted = result.is_ok();
                self.respond(conn, &respond_now(result));
                // A selection from it reaches this conn either in its catch-up or live, whenever the probe ends.
                if let Some(cwd) = hello.cwd.filter(|_| accepted) {
                    self.dispatch(HostEvent::SelectWorkspace {
                        ask: WorkspaceAsk::ClientCwd,
                        path: PathBuf::from(cwd),
                    });
                }
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
                // The hooks of the workspace the session was created in, as the config says now.
                let hooks = match self.host.session_workspace(&close.session) {
                    Some(workspace) => self.settings.config.workspace_hooks(workspace),
                    None => WorkspaceHooks::default(),
                };
                self.dispatch(HostEvent::Close {
                    reply,
                    session: close.session,
                    worktree: close.worktree,
                    hooks,
                });
            }
            ClientRequest::WorktreeStatus(check) => {
                let reply = self.reply_to(conn, id);
                self.dispatch(HostEvent::WorktreeStatus {
                    reply,
                    session: check.session,
                });
            }
            ClientRequest::WorkspaceSelect(select) => {
                let reply = self.reply_to(conn, id);
                self.dispatch(HostEvent::SelectWorkspace {
                    ask: WorkspaceAsk::Select(reply),
                    path: expand_home(Path::new(&select.path), &self.settings.home),
                });
            }
            ClientRequest::HostUpgrade(HostUpgrade { force }) => {
                let reply = self.reply_to(conn, id);
                if self.upgrade.is_some() {
                    let reason = "an update is already being checked".to_owned();
                    return self.reply(
                        reply,
                        Ok(HostResult::Upgrade(UpgradeAnswer::Busy { reason })),
                    );
                }
                self.upgrade = Some(PendingUpgrade {
                    reply,
                    force,
                    preflight: None,
                });
                self.advance_upgrade();
            }
            ClientRequest::SessionResume(resume) => {
                let reply = self.reply_to(conn, id);
                let session = resume.session;
                let (spec, files) = match self.plan_resume(&session) {
                    Ok(planned) => planned,
                    Err(error) => return self.reply(reply, Err(error)),
                };
                if !files.is_empty() {
                    self.pending_run_files.insert(session.clone(), files);
                }
                self.dispatch(HostEvent::Resume {
                    reply,
                    session: session.clone(),
                    spec,
                });
                // Left over only if core refused the resume and nothing was spawned.
                self.pending_run_files.remove(&session);
            }
        }
    }

    // ---------- updating in place ----------

    /// Decides the pending upgrade request with what is known now: answers it, starts its preflight, or swaps.
    fn advance_upgrade(&mut self) {
        let Some(pending) = &self.upgrade else {
            return;
        };
        let installed = self.settings.installed.digest();
        let failed = self
            .swap_failed
            .as_ref()
            .filter(|(at, _)| *at == installed)
            .map(|(_, why)| why.clone());
        let facts = UpgradeFacts {
            running: self.settings.build.as_ref().map(|b| b.digest),
            installed,
            force: pending.force,
            work: Work {
                port_calls: self.worktree_calls + self.hook_calls,
                run_files: self.pending_run_files(),
            },
            failed,
            preflight: pending.preflight.clone(),
        };
        match decide(&facts) {
            UpgradeStep::Preflight => self.start_preflight(),
            UpgradeStep::Answer(UpgradeAnswer::Accepted) => self.begin_swap(),
            UpgradeStep::Answer(answer) => self.answer_upgrade(answer),
        }
    }

    fn answer_upgrade(&mut self, answer: UpgradeAnswer) {
        if let Some(pending) = self.upgrade.take() {
            self.reply(pending.reply, Ok(HostResult::Upgrade(answer)));
        }
    }

    /// Runs the preflight on a thread of its own; its result comes back as [`RuntimeEvent::Preflighted`].
    fn start_preflight(&mut self) {
        let runner = self.settings.preflight.clone();
        let exe = self.settings.shepherd.clone();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name("preflight".into())
            .spawn(move || tx.send(RuntimeEvent::Preflighted(runner.run(&exe))));
        match spawned {
            Ok(_) => self.preflighting = true,
            Err(e) => {
                let preflight = Preflight::Failed(format!("cannot start a thread: {e}"));
                if let Some(pending) = &mut self.upgrade {
                    pending.preflight = Some(preflight);
                }
                self.advance_upgrade();
            }
        }
    }

    /// The upgrade is accepted: hands the PTYs over, says so, takes in what is queued and readies the plan for
    /// [`HostRuntime::run`] to hand out. The conns are closed by [`HostRuntime::close_conns`], once the caller has
    /// stopped accepting new ones.
    fn begin_swap(&mut self) {
        let tokens = match self.pty.export() {
            Ok(tokens) => tokens,
            Err(error) => {
                let reason = format!("cannot hand the sessions over: {error}");
                return self.answer_upgrade(UpgradeAnswer::Unavailable { reason });
            }
        };
        self.answer_upgrade(UpgradeAnswer::Accepted);
        self.swapping = true;
        tracing::info!(sessions = tokens.len(), "updating in place");
        // The pumps are paused, so this ends: what they read is queued, and only the screens and hook frames take it.
        while let Ok(event) = self.rx.0.try_recv() {
            self.handle(event);
        }
        let sessions = tokens
            .into_iter()
            .map(|(id, token)| {
                let (cols, rows, snapshot) = match self.screens.get(&id) {
                    Some(screen) => {
                        let (cols, rows) = screen.size();
                        (cols, rows, screen.snapshot_ansi())
                    }
                    None => (INITIAL_SIZE.cols, INITIAL_SIZE.rows, Vec::new()),
                };
                CarriedSession {
                    id,
                    cols,
                    rows,
                    snapshot,
                    token,
                }
            })
            .collect();
        self.swap = Some(SwapPlan {
            sessions,
            translators: self.translators.carry(),
        });
    }

    /// What a new session runs, and the run files to write before spawning it: the named profile, else
    /// `[defaults] agent`, in the requested directory, else the workspace's root, else the home directory, at
    /// [`INITIAL_SIZE`].
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
        let cwd = match (new.cwd.as_deref(), self.host.workspace()) {
            (Some(cwd), _) => existing_dir(Path::new(cwd))?,
            // Checked here too, to say which workspace is gone; the spawn checks every cwd again.
            (None, Some(workspace)) => existing_dir(workspace).map_err(|_| {
                WireError::InvalidParams(format!(
                    "the workspace {} no longer exists; choose another",
                    workspace.display()
                ))
            })?,
            (None, None) => self.settings.home.clone(),
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
        let hooks = match self.host.workspace() {
            Some(workspace) => config.workspace_hooks(workspace),
            None => WorkspaceHooks::default(),
        };
        let spec = NewSessionSpec {
            name: None,
            profile: profile.name.clone(),
            agent: profile.kind,
            explicit_cwd: new.cwd.is_some(),
            spawn: launch.spawn,
            hooks,
        };
        Ok((spec, launch.files))
    }

    /// What resuming `session` runs, and the run files to write before spawning it: its profile, as it is in the
    /// config now, planned with the agent session to resume in the directory it started in (see
    /// [`Host::resume_target`]), which must still be a directory. Core checks the profile is still the kind the
    /// session started with.
    fn plan_resume(&self, session: &SessionId) -> Result<(ResumeSpec, Vec<RunFile>), WireError> {
        let target = match self.host.resume_target(session) {
            None => {
                return Err(WireError::UnknownSession {
                    session: session.clone(),
                });
            }
            Some(Err(why)) => {
                return Err(WireError::InvalidParams(format!(
                    "can't resume {session}: {why}"
                )));
            }
            Some(Ok(target)) => target,
        };
        let profile = self
            .settings
            .config
            .profile(&target.profile)
            .ok_or_else(|| {
                WireError::InvalidParams(format!(
                    "can't resume {session}: profile `{}` no longer exists",
                    target.profile
                ))
            })?;
        // Checked here too, so the client hears why it can't resume; the spawn checks every cwd again.
        let cwd = existing_dir(&target.cwd).map_err(|_| {
            WireError::InvalidParams(format!(
                "can't resume {session}: {} no longer exists",
                target.cwd.display()
            ))
        })?;
        let ctx = LaunchContext {
            session: session.clone(),
            cwd,
            size: INITIAL_SIZE,
            run_dir: run_files::session_dir(&self.settings.run_root, session),
            shepherd: self.settings.shepherd.clone(),
            socket: self.settings.socket.clone(),
        };
        let launch = launch::plan_resume(profile, &ctx, &target.agent_session_id)
            .map_err(|e| WireError::InvalidParams(format!("can't resume {session}: {e}")))?;
        let spec = ResumeSpec {
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
        // A create has no time cap: one left running would outlive the host. Its answer, if it comes before the loop
        // stops, is dropped like any other.
        for (repo, name) in self.creating.values() {
            self.worktrees.port().cancel_create(repo, name);
        }
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

/// The root of the repo `path` is in ([`Worktrees::repo_root`]: a subdirectory or a linked worktree names its main
/// checkout), resolved, if `path` is a directory and the repo's default branch can be found; or why it can't be a
/// workspace, for the user. Runs on the worker thread: it touches the filesystem and git.
fn probe_workspace(worktrees: &impl Worktrees, path: &Path) -> Result<PathBuf, String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("{shown} is not an absolute path"));
    }
    let resolved = std::fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
    if !resolved.is_dir() {
        return Err(format!("{shown} is not a directory"));
    }
    let root = worktrees
        .repo_root(&resolved)
        .map_err(|e| format!("{shown}: {e}"))?;
    worktrees
        .default_branch(&RepoRef::new(root.clone()))
        .map_err(|e| format!("{shown}: {e}"))?;
    Ok(root)
}

/// Whether `path` is one of `repo`'s managed worktrees, at exactly the path the port lists, and if so its status; and
/// whether anything is there at all (without following a symlink). Runs on the worker thread.
fn inspect_worktree(
    worktrees: &impl Worktrees,
    repo: &RepoRef,
    path: &WorktreePath,
) -> Result<Inspection, WorktreeError> {
    let listed = worktrees.list_managed(repo)?;
    if listed.iter().any(|w| w.path == *path) {
        let status = worktrees.status(path);
        let exists = anything_at(path);
        return Ok(Inspection::Listed { status, exists });
    }
    let exists = anything_at(path);
    Ok(Inspection::Unlisted { exists })
}

/// Whether anything (a symlink included, not followed) is at `path`.
fn anything_at(path: &WorktreePath) -> bool {
    std::fs::symlink_metadata(path.as_path()).is_ok()
}

/// A session's screen panicked and was replaced with a blank one ([`screen_guard`]).
fn log_screen_panic(session: &SessionId, result: Result<(), String>) {
    if let Err(panic) = result {
        tracing::error!(%session, %panic, "screen panicked; replaced it with a blank one");
    }
}

/// `cwd` as a directory a session can start in: absolute, since a client can't know where the host started, and an
/// existing directory.
fn existing_dir(path: &Path) -> Result<PathBuf, WireError> {
    let cwd = path.display();
    if !path.is_absolute() {
        return Err(WireError::InvalidParams(format!(
            "cwd `{cwd}` is not an absolute path"
        )));
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(path.to_path_buf()),
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
