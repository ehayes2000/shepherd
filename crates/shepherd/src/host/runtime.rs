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
//!   blocked catch-up retries after [`CATCH_UP_TICK`], doubling up to [`MAX_CATCH_UP_TICK`] while nothing of its
//!   queue is delivered. Entries are read [`CATCH_UP_BATCH`] at a time (`EventLog::read_batch`), so each is read once
//!   and a conn holds at most one batch. A conn the link has delivered nothing to for [`STALL_TIMEOUT`]
//!   (`Link::drained_bytes` hasn't moved, so one huge line on its way over a slow link is progress) is closed: a client
//!   that stopped reading. A log whose cursor hasn't reached its end gets no live
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
//!   running create and waits (within [`CREATE_CLEANUP_GRACE`]) for its answer, which core records, so a create
//!   stopped by the shutdown never leaves a worktree or branch behind unrecorded. The `Commands` port (a worktree's
//!   post-create and pre-remove hooks) runs each hook on a thread of its own the same way, its results coming back as
//!   [`RuntimeEvent::Commands`], so a long hook never holds up git
//!   calls, the loop, or another session's hooks. Closing a session whose hook is running cancels it
//!   (`Commands::cancel`, which kills its process group and never waits), and its answer comes back as usual; the
//!   close's inspection of the worktree waits for that answer, so it sees what the hook left. Shutting the host down
//!   cancels every running hook and waits (within the shutdown grace) for their answers, which it drops: a session
//!   stopping with the host spawns nothing. From then on no hook starts and no PTY spawns (a close's inspection or a
//!   create's result that arrives later would otherwise start one nothing stops).
//! - **Captures** (T42). Core's [`HostCommand::Capture`]s run on the `Archive` port's [`Worker`], in order, stamped with
//!   `Env::now` when submitted: each finds the repo's root commit and the session's commits since its base through the
//!   `Worktrees` port (shared with its worker) and then records the session and snapshots its transcript. The answer
//!   comes back as [`RuntimeEvent::Archive`] and goes to core, which removes a worktree only once its capture worked.
//!   Each agent's home is the one its session's profile env gives it ([`AgentHomes::launched_with`]), else the host's.
//!   An agent running outside any git repo can't be archived (no repo to file it under): its captures are skipped and
//!   that is logged once per session ([`Archived::outside_repo`]); a capture before a removal fails instead.
//!   Bounded: a turn end's capture is dropped while another of that agent session's waits to start (the waiting one
//!   reads the transcript as it is by then). Shutting down waits for captures running (within
//!   [`CREATE_CLEANUP_GRACE`]) and drops their answers: a removal that would follow is left to the next start, which keeps the worktree. An upgrade waits
//!   for them too (`Busy`).
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
//!   The caller stops accepting, closes every conn ([`HostRuntime::close_conns`], which waits, bounded, for what was
//!   queued to be delivered) and re-execs; if that fails, [`HostRuntime::swap_failed`] resumes the PTYs and
//!   later upgrades are answered with why. The new image starts with [`HostRuntime::adopt`]: adopted sessions get no
//!   exit and keep their run directory, their screens are fed their snapshots, the translators are restored, and each
//!   is nudged to redraw.
//! - **Quiet watch.** A [`QuietWatch`] (the quiet-output idle fallback) sees every entry appended, and the logs on
//!   start. Output of, and client input to, the sessions it watches are fed to it stamped with `Env::now`; its
//!   deadline is serviced like the others: when it is due, a tick stamped with `Env::now` returns the idles to append.
//! - **PR watch.** A [`PrWatch`] (T27) sees every entry appended, and the logs on start. Its deadline is serviced the
//!   same way: a tick stamped with `Env::now` returns the `Forge` calls to make, which run on a worker of their own (each
//!   bounded, since `gh` waits on the network), their answers coming back as [`RuntimeEvent::Forge`] and going to the
//!   watch, which returns the `session/pr` frames to append. An upgrade doesn't wait for them: the new image polls again.
//! - **Peers** (T30). A [`Peering`] beside core decides pairing and trust: it sees every entry appended, and the logs
//!   on start (so a host that trusts someone binds and dials them at once). The pairing and peer requests go to it,
//!   with a fresh token from `Env::pair_token` for a pairing, and so do the `Peers` port's events. After each batch of
//!   its commands, its `sync` pushes what the batch changed (the trusted set, binding). Its deadline (a pairing's expiry)
//!   is serviced like the others. Shutting down unbinds, and so does a swap before the exec (a failed exec binds again).
//! - **Tunnels** (T31a, decision 0019). A conn from one of this machine's clients whose first request is
//!   `_shepherd/tunnel` (core's [`Peering::tunnel`] decides) gets a conn [opened](Link::open) to that machine's host,
//!   and from then on the two are spliced: each line read from one is sent to the other unchanged. The tunnel request
//!   is answered once that host accepted the conn, or with why not ([`Peering::tunnel_failed`]), closing the client's
//!   conn. Flow control: a line goes only while the other end's queue has room under [`TUNNEL_HIGH`] (a line into an
//!   empty queue always goes), else it is held, and the end it came from stops being read
//!   (`Link::pause_reading`) until what it holds is sent and the other end's queue is under [`TUNNEL_LOW`], checked
//!   after [`TUNNEL_TICK`], doubling up to [`MAX_TUNNEL_TICK`] while no stopped end makes progress (a line sent, its
//!   other end's queue shrinking, or reading resumed), so a client that stopped reading costs few wakeups. So a slow client backs up into the conn to the other machine's host, whose own catch-up
//!   pacing (decision 0013) then waits, and the other way round. Either end going closes the other, after sending
//!   what it held: a client sees `PeerGone` when the other machine goes, and can reconnect. Lines read from an end
//!   before it stopped being read are held, up to [`TUNNEL_HELD_MAX`] bytes: past that the other end is a slow
//!   consumer and both ends are closed, logged with why. At most
//!   [`MAX_OPEN_PER_PEER`](shepherd_core::ports::MAX_OPEN_PER_PEER) tunnels to one machine are open
//!   at once (the most its one connection carries); one more is refused at once.
//! - **Local only.** Pairing, trust and updating the host (core's [`peering::refused_to_peers`]) are refused on another
//!   machine's conn, like a tunnel through this host; a conn the link no longer knows is refused too, never taken for
//!   a local one.
//! - **Spawn sizes.** Every event goes to core through a [`Sizes`], which keeps the latest size each session's terminal
//!   was given (a client's resize, even before its PTY exists, or an adopted session's carried size) and spawns its
//!   PTY at it. A new session's spec asks for the client's view size from its `session/new`, else [`INITIAL_SIZE`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use shepherd_core::acpp::{
    Envelope, Frame, FrameDraft, RequestId, Seq, ShepherdFrame, SourceKind, Stamp, Timestamp,
};
use shepherd_core::agents::{CarriedTranslators, Translators};
use shepherd_core::config::{AgentKind, Config, WorkspaceHooks};
use shepherd_core::host::capture::{AfterCapture, AgentHomes, CaptureJob, Found};
use shepherd_core::host::launch::{self, LaunchContext, RunFile};
use shepherd_core::host::peering::{self, Peering, PeeringEvent, PeersCommand};
use shepherd_core::host::pr::{PrEvent, PrWatch};
use shepherd_core::host::quiet::{QuietEvent, QuietWatch};
use shepherd_core::host::sizes::Sizes;
use shepherd_core::host::upgrade::{Preflight, UpgradeFacts, UpgradeStep, Work, decide};
use shepherd_core::host::{
    Host, HostCommand, HostEvent, Inspect, InspectedOrphans, Inspection, NewSessionSpec, Remover,
    ReplyTo, ResumeSpec, WorkspaceAsk, expand_home,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    Archive, ArchiveError, CLOSE_FLUSH, CaptureReason, CommandError, Commands, ConnId,
    DisconnectReason, EventLog, FlushOutcome, Forge, ForgeError, Link, LinkEvent, LinkEventSink,
    LogError, LogId, MAX_QUEUED, PairTicket, Peers, PeersEvent, PeersEventSink, PtyError, PtyEvent,
    PtyEventSink, PtyHandoff, PtySize, RepoRef, RunId, WorktreeError, WorktreeName, WorktreePath,
    Worktrees,
};
use shepherd_core::wire::{
    self, BuildId, ClientMessage, ClientNotification, ClientRequest, ExeDigest, HookFrame,
    HostNotification, HostResponse, HostResult, HostUpgrade, LogEntry, PtyOutput, SessionNew,
    SubscribeFrom, UpgradeAnswer, WireError,
};
use shepherd_term::{MAX_COLS, MAX_ROWS, MIN_COLS, MIN_ROWS, Screen};

use super::env::Env;
use super::run_files;
use super::worker::Worker;
use crate::build::InstalledExe;
use crate::screen_guard;

/// The size a session's PTY starts at when no size is known for it: its `session/new` gave none and no client has
/// resized it (see [`Sizes`]).
pub const INITIAL_SIZE: PtySize = PtySize { cols: 80, rows: 24 };
/// How long shutdown waits for killed PTYs to report their exit before giving up on them.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long shutdown waits, instead, when a worktree create is running: the create, cancelled, kills its git and undoes
/// what it made (deleting a big checkout takes a while), and a host that stopped first would leave that half made and
/// unrecorded.
pub const CREATE_CLEANUP_GRACE: Duration = Duration::from_secs(30);
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
/// A tunnel sends a line to one end only while that end's queue stays under this (or is empty); otherwise the other
/// end stops being read.
pub const TUNNEL_HIGH: usize = MAX_QUEUED / 4;
/// A tunnel end stopped for its other end's sake is read again once that end's queue is under this.
pub const TUNNEL_LOW: usize = TUNNEL_HIGH / 2;
/// The most bytes a tunnel end holds for its other end (lines read from it before it stopped being read, decision
/// 0021): past this the other end is a slow consumer, as a link would drop it past [`MAX_QUEUED`], and the tunnel is
/// closed on both ends.
pub const TUNNEL_HELD_MAX: usize = MAX_QUEUED;
/// How soon a tunnel that stopped reading an end first looks again.
pub const TUNNEL_TICK: Duration = Duration::from_millis(5);
/// The longest wait between looks at stopped tunnel ends that make no progress.
pub const MAX_TUNNEL_TICK: Duration = MAX_CATCH_UP_TICK;

/// What a capture on the archive's worker came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    /// Its [`HostEvent::Captured`], for core.
    pub event: HostEvent,
    /// The sessions it skipped because their agent runs outside any git repo (their `cwd`): with no repo to file it
    /// under, such a conversation can't be archived. Never a capture before a removal, which fails instead.
    pub outside_repo: Vec<(SessionId, WorktreePath)>,
}

/// Everything the loop reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Pty(PtyEvent),
    Link(LinkEvent),
    /// The result of a `Worktrees` call that ran on the worker thread, for core.
    Worktrees(HostEvent),
    /// The result of a `Commands` call (a hook) that ran on its worker thread, for core.
    Commands(HostEvent),
    /// The answer to a `Forge` call that ran on its worker thread, for the PR watch.
    Forge(PrEvent),
    /// The answer to a capture (`Archive` and `Worktrees` calls) that ran on its worker thread.
    Archive(Archived),
    /// SIGTERM or SIGINT: kill the PTYs, record their exits, close the links, stop.
    Shutdown,
    /// How the upgrade preflight (`<shepherd> handoff-check`) ended.
    Preflighted(Preflight),
    /// What the `Peers` port reports, for the peering.
    Peers(PeersEvent),
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
            // The port's error says whether it killed the session; a live id always is.
            Err(PtyError::AlreadyExists) => {
                tracing::warn!(session = %id, "cannot adopt a session already live; killed it")
            }
            Err(error) => tracing::warn!(session = %id, %error, "cannot adopt session"),
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

    /// A sink for a [`Peers`] adapter, forwarding into this channel.
    pub fn peers_sink(&self) -> Arc<dyn PeersEventSink> {
        Arc::new(self.clone())
    }
}

impl PeersEventSink for RuntimeTx {
    fn send(&self, event: PeersEvent) {
        RuntimeTx::send(self, RuntimeEvent::Peers(event));
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
    /// Something every client's hello tells the user, e.g. that the config couldn't be used after an update.
    pub warning: Option<String>,
    /// This machine's id, from its `MachineIdentity` key: every entry is stamped with it.
    pub machine: MachineId,
    /// Where each agent keeps its transcripts, for captures.
    pub agent_homes: AgentHomes,
    /// The hostname as the system reports it, which names the machine when the config doesn't
    /// ([`shepherd_core::machine::name`]).
    pub hostname: Option<String>,
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
    /// A line came from it: a tunnel request now isn't its first.
    spoke: bool,
}

/// One end of a tunnel (see the module docs).
#[derive(Debug)]
struct TunnelEnd {
    /// The conn at the other end.
    other: ConnId,
    /// Lines read from this end, waiting for room in the other end's queue.
    held: VecDeque<Vec<u8>>,
    /// The bytes in `held`, at most [`TUNNEL_HELD_MAX`].
    held_bytes: usize,
    /// This end isn't being read, for the other end's sake.
    paused: bool,
    /// The other end's queue when this end was last looked at.
    last_queued: usize,
    /// The client's end of a tunnel not open yet: its request, and the machine it asked for.
    opening: Option<(RequestId, MachineId)>,
    /// The end this host opened to another machine's host: that machine, whose connection carries it.
    to: Option<MachineId>,
}

impl TunnelEnd {
    fn new(other: ConnId) -> Self {
        Self {
            other,
            held: VecDeque::new(),
            held_bytes: 0,
            paused: false,
            last_queued: 0,
            opening: None,
            to: None,
        }
    }
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
    /// What the link had delivered of the conn's queue at the last check. More at the next check is progress, even
    /// if the queue didn't shrink (live traffic grew it, or one huge line is on its way).
    drained: u64,
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
pub struct HostRuntime<P: PtyHandoff, W, C, F, L, E, V, N, A> {
    host: Host,
    pty: P,
    worktrees: Worker<W>,
    /// `Worktrees` calls submitted and not yet answered.
    worktree_calls: usize,
    commands: Worker<C>,
    /// `Commands` calls (hooks) running and not yet answered, by session (a session runs one hook at a time), for
    /// `Commands::cancel`.
    hooks: HashMap<SessionId, RunId>,
    /// Pre-remove hooks of orphan worktrees the cleanup view is removing, running and not yet answered, by the request
    /// they answer, so shutdown can cancel them too.
    cleanup_hooks: HashMap<ReplyTo, RunId>,
    /// A close's inspection of a session whose hook (cancelled by that close) hasn't answered yet: it starts once the
    /// hook has, so it sees what the hook left.
    after_hook: HashMap<SessionId, HostCommand>,
    forge: Worker<F, PrEvent>,
    /// `Forge` calls submitted and not yet answered.
    forge_calls: usize,
    archive: Worker<A, Archived>,
    /// `Archive` calls (captures) submitted and not yet answered.
    capture_calls: usize,
    /// Agent sessions (a session and its agent's own session id) with a turn end's capture queued and not started
    /// yet: another turn end of theirs adds nothing, since that capture reads the transcript once it starts. So at
    /// most one per agent session waits on the worker; a new agent session in place (`/clear`) has its own.
    queued_turn_ends: Arc<Mutex<HashSet<(SessionId, String)>>>,
    /// Sessions already logged as running outside a git repo, so unarchivable: said once per session, not every turn.
    outside_repo: HashSet<SessionId>,
    link: L,
    peers: N,
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
    /// The latest size each session's terminal was given, which its PTY spawns at.
    sizes: Sizes,
    quiet: QuietWatch,
    /// When the quiet watch's deadline is due on the monotonic clock.
    quiet_due: Option<Instant>,
    pr: PrWatch,
    /// When the PR watch's deadline is due on the monotonic clock.
    pr_due: Option<Instant>,
    /// The wall-clock and monotonic time of the PR watch's last tick, to schedule from when an answer comes back.
    pr_ticked: Option<(Timestamp, Instant)>,
    upgrade: Option<PendingUpgrade>,
    /// The preflight is running.
    preflighting: bool,
    /// An accepted upgrade has exported the PTYs: only hook frames are still taken.
    swapping: bool,
    /// A swap ready to exec, for [`HostRuntime::run`] to hand out.
    swap: Option<SwapPlan<P::Token>>,
    /// Why the last accepted swap failed, and what was installed then.
    swap_failed: Option<(Option<ExeDigest>, String)>,
    peering: Peering,
    /// When the peering's deadline (a pairing's expiry) is due on the monotonic clock.
    peer_due: Option<Instant>,
    /// Both ends of every tunnel, by conn.
    tunnels: HashMap<ConnId, TunnelEnd>,
    /// When to look again at tunnel ends holding lines or stopped.
    tunnel_due: Option<Instant>,
    /// How long the wait before that look was.
    tunnel_wait: Duration,
}

impl<
    P: PtyHandoff,
    W: Worktrees + Send + Sync + 'static,
    C: Commands + Send + Sync + 'static,
    F: Forge + Send + Sync + 'static,
    L: Link,
    E: EventLog,
    V: Env,
    N: Peers,
    A: Archive + Send + Sync + 'static,
> HostRuntime<P, W, C, F, L, E, V, N, A>
{
    /// Rebuilds the host state by folding every log, then records an exit for every session that was still
    /// running: PTYs never survive a host restart. `tx` and `rx` are the channel the ports' sinks feed. `worktrees`,
    /// `commands`, `forge` and `archive` each move to a worker thread of their own.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        pty: P,
        worktrees: W,
        commands: C,
        forge: F,
        archive: A,
        link: L,
        peers: N,
        log: E,
        env: V,
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
    ) -> Result<Self, StartError> {
        let ports = (
            pty, worktrees, commands, forge, archive, link, peers, log, env,
        );
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
        forge: F,
        archive: A,
        link: L,
        peers: N,
        log: E,
        env: V,
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
        adoption: Adoption,
    ) -> Result<Self, StartError> {
        let ports = (
            pty, worktrees, commands, forge, archive, link, peers, log, env,
        );
        Self::start_with(ports, settings, tx, rx, adoption)
    }

    #[allow(clippy::type_complexity)]
    fn start_with(
        (pty, worktrees, commands, forge, archive, link, peers, log, env): (
            P,
            W,
            C,
            F,
            A,
            L,
            N,
            E,
            V,
        ),
        settings: RuntimeSettings,
        tx: RuntimeTx,
        rx: RuntimeRx,
        adoption: Adoption,
    ) -> Result<Self, StartError> {
        let mut host = Host::new();
        let mut quiet = QuietWatch::new(&settings.config);
        let mut pr = PrWatch::new();
        let mut peering = Peering::new();
        for id in log.logs()? {
            for envelope in log.read(id, Seq::ZERO)? {
                host.apply(&resolved_paths(&envelope));
                quiet.apply(&envelope);
                pr.apply(&envelope);
                peering.apply(&envelope);
            }
        }
        let worktrees = Worker::spawn("worktrees", worktrees, tx.clone(), RuntimeEvent::Worktrees)
            .map_err(StartError::Worker)?;
        let commands = Worker::spawn("hooks", commands, tx.clone(), RuntimeEvent::Commands)
            .map_err(StartError::Worker)?;
        let forge = Worker::spawn("forge", forge, tx.clone(), RuntimeEvent::Forge)
            .map_err(StartError::Worker)?;
        let archive = Worker::spawn("archive", archive, tx.clone(), RuntimeEvent::Archive)
            .map_err(StartError::Worker)?;
        let machine = settings.machine.clone();
        let mut runtime = Self {
            host,
            pty,
            worktrees,
            worktree_calls: 0,
            commands,
            hooks: HashMap::new(),
            cleanup_hooks: HashMap::new(),
            after_hook: HashMap::new(),
            forge,
            forge_calls: 0,
            archive,
            capture_calls: 0,
            queued_turn_ends: Arc::default(),
            outside_repo: HashSet::new(),
            link,
            peers,
            log,
            env,
            settings,
            machine,
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
            sizes: Sizes::new(),
            quiet,
            quiet_due: None,
            pr,
            pr_due: None,
            pr_ticked: None,
            upgrade: None,
            preflighting: false,
            swapping: false,
            swap: None,
            swap_failed: None,
            peering,
            peer_due: None,
            tunnels: HashMap::new(),
            tunnel_due: None,
            tunnel_wait: TUNNEL_TICK,
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
        let name = shepherd_core::machine::name(
            &runtime.settings.config,
            runtime.settings.hostname.as_deref(),
        );
        runtime.dispatch(HostEvent::MachineNamed { name });
        // Every other run directory is stale: the log lock is held and nothing else has been spawned yet.
        run_files::sweep_except(&runtime.settings.run_root, &adopted);
        runtime.restore(adoption);
        // Polled sessions are looked at on the first iteration, which reads the clock and schedules what is due later.
        if runtime.pr_due.is_none() && runtime.pr.deadline().is_some() {
            runtime.pr_due = Some(runtime.env.instant());
        }
        // A host that trusts someone binds now, and dials them; one that never paired stays offline.
        let sync = runtime.peering.sync();
        runtime.execute_all(sync);
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
            self.sizes.adopted(id.clone(), PtySize { cols, rows });
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
            .chain(self.pr_due)
            .chain(self.peer_due)
            .chain(self.tunnel_due)
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
    /// backlog for the new image instead of being accepted by this one and cut off by the exec. Returns once what was
    /// sent to each, the upgrade's answer included, is delivered, or after [`CLOSE_FLUSH`] for a client that isn't
    /// reading: the exec would cut the deliveries off. The one wait in the loop, bounded, and only for a swap.
    ///
    /// Then it takes this machine offline ([`Peering::suspend`]), so peers see its connections close at once rather
    /// than time out; the new image binds again as it starts, with the same identity, and [`HostRuntime::swap_failed`]
    /// binds again if the exec fails. The unbind is asked of the port here; waiting until it is done is the caller's
    /// (the real port does it on a thread of its own).
    pub fn close_conns(&mut self) {
        let conns: Vec<ConnId> = self
            .conns
            .keys()
            .chain(self.tunnels.keys())
            .copied()
            .collect();
        self.tunnels.clear();
        self.tunnel_due = None;
        for conn in conns {
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
        self.conns.clear();
        self.replies.clear();
        // Timed by the link, on the clock it waits with: not `Env::instant`, which tests move by hand.
        if self.link.flush_closed(CLOSE_FLUSH) == FlushOutcome::TimedOut {
            tracing::warn!("a client didn't read what it was sent before the swap; it is cut off");
        }
        let offline = self.peering.suspend();
        self.execute_all(offline);
        self.peer_due = None;
    }

    /// The exec of a swap failed with `why`: the PTYs are resumed and served as before, the machine comes back online,
    /// and the next upgrade request is answered with why, until another build is installed. The conns closed for the
    /// swap stay closed; clients reconnect.
    pub fn swap_failed(&mut self, why: String) {
        tracing::error!(%why, "updating in place failed; carrying on");
        self.pty.resume();
        self.swapping = false;
        self.swap = None;
        let installed = self.settings.installed.digest();
        self.swap_failed = Some((installed, why));
        let online = self.peering.resume();
        self.execute_all(online);
    }

    /// The `Peers` port, e.g. for the caller of [`HostRuntime::close_conns`] to wait until it is offline.
    pub fn peers(&self) -> &N {
        &self.peers
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

    /// [`HostRuntime::run_until_idle`], and then, while a `Worktrees`, `Commands` or `Forge` call is still unanswered, waits for
    /// the next event and goes on. For tests. `limit` bounds each wait, for liveness; false if one ran out.
    pub fn run_until_settled(&mut self, limit: Duration) -> bool {
        loop {
            self.run_until_idle();
            let calls = self.worktree_calls
                + self.hooks.len()
                + self.cleanup_hooks.len()
                + self.forge_calls
                + self.capture_calls;
            if calls == 0 && !self.preflighting || self.finished {
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
                if self.tunnels.contains_key(&conn) {
                    self.tunnel_open(conn);
                } else {
                    self.conns.insert(conn, ConnState::default());
                }
            }
            // Lines while shutting down are dropped.
            RuntimeEvent::Link(LinkEvent::Line { .. }) if self.shutdown_by.is_some() => {}
            RuntimeEvent::Link(LinkEvent::Line { conn, line }) => {
                if self.tunnels.contains_key(&conn) {
                    self.tunnel_line(conn, line);
                } else {
                    self.client_line(conn, &line);
                }
            }
            RuntimeEvent::Link(LinkEvent::Disconnected { conn, reason }) => {
                tracing::debug!(%conn, ?reason, "client disconnected");
                if self.tunnels.contains_key(&conn) {
                    self.tunnel_gone(conn, &reason);
                } else {
                    self.forget(conn);
                }
            }
            RuntimeEvent::Worktrees(event) => {
                self.worktree_calls = self.worktree_calls.saturating_sub(1);
                match &event {
                    HostEvent::WorktreeCreated { session, .. } => {
                        let session = session.clone();
                        self.worktree_created(session, event);
                        // The last create a shutdown waited for.
                        self.check_shutdown_done();
                    }
                    _ => self.dispatch(event),
                }
            }
            RuntimeEvent::Commands(event) => {
                let session = match &event {
                    HostEvent::HookFinished { session, .. }
                    | HostEvent::PreRemoveFinished {
                        remover: Remover::Session(session),
                        ..
                    } => Some(session.clone()),
                    HostEvent::PreRemoveFinished {
                        remover: Remover::Orphan { reply, .. },
                        ..
                    } => {
                        self.cleanup_hooks.remove(reply);
                        None
                    }
                    _ => None,
                };
                if let Some(session) = &session {
                    self.hooks.remove(session);
                }
                // A hook cancelled by the shutdown: its session stops with the host, so nothing follows from it.
                if self.shutdown_by.is_some() {
                    self.check_shutdown_done();
                    return;
                }
                match &event {
                    HostEvent::HookFinished { session, .. } => {
                        let session = session.clone();
                        self.worktree_created(session, event);
                    }
                    _ => self.dispatch(event),
                }
                // The hook is gone: a close waiting for it inspects the worktree now.
                if let Some(inspect) = session.and_then(|s| self.after_hook.remove(&s)) {
                    self.run_commands(vec![inspect]);
                }
            }
            RuntimeEvent::Forge(answer) => {
                self.forge_calls = self.forge_calls.saturating_sub(1);
                // Taken even while swapping or shutting down, so the session isn't left waiting on it if the swap fails.
                let commands = self.pr.handle(answer);
                self.execute_all(commands);
                // The session answered is due again: schedule from the tick that asked.
                if let Some((at, then)) = self.pr_ticked.clone() {
                    self.pr_due = self
                        .pr
                        .deadline()
                        .map(|deadline| then + until(&at, deadline));
                }
            }
            RuntimeEvent::Archive(Archived {
                event,
                outside_repo,
            }) => {
                self.capture_calls = self.capture_calls.saturating_sub(1);
                for (session, cwd) in outside_repo {
                    if self.outside_repo.insert(session.clone()) {
                        tracing::warn!(
                            %session,
                            %cwd,
                            "the agent runs outside a git repo: its conversation can't be archived"
                        );
                    }
                }
                if let HostEvent::Captured {
                    result: Err(error), ..
                } = &event
                {
                    tracing::warn!(%error, "archiving a conversation failed");
                }
                // A capture the shutdown waited for: what would follow (a removal) is left for the next start, which
                // keeps the worktree of a session still closing.
                if self.shutdown_by.is_some() {
                    self.check_shutdown_done();
                    return;
                }
                self.dispatch(event);
            }
            RuntimeEvent::Shutdown => self.start_shutdown(),
            RuntimeEvent::Preflighted(preflight) => {
                self.preflighting = false;
                if let Some(pending) = &mut self.upgrade {
                    pending.preflight = Some(preflight);
                    self.advance_upgrade();
                }
            }
            RuntimeEvent::Peers(event) => {
                log_peers_event(&event);
                let at = self.env.now();
                self.peering(PeeringEvent::Peers { event, at });
            }
        }
    }

    // ---------- core events and commands ----------

    /// Runs `event` through core and executes what it returns, then any events that execution produced.
    fn dispatch(&mut self, event: HostEvent) {
        let commands = self.decide(event);
        self.run_commands(commands);
    }

    /// Executes `commands`, then runs any events that execution produced through core, as [`Self::dispatch`] does.
    fn run_commands(&mut self, commands: Vec<HostCommand>) {
        let mut queue = VecDeque::new();
        self.execute(commands, &mut queue);
        while let Some(event) = queue.pop_front() {
            let commands = self.decide(event);
            self.execute(commands, &mut queue);
        }
    }

    /// What core decides about `event`. Every event goes through [`Sizes`], so a spawn gets its session's latest size.
    fn decide(&mut self, event: HostEvent) -> Vec<HostCommand> {
        self.sizes.handle(&self.host, event)
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
                    turn_end,
                } => {
                    if let Err(error) = self.translate_hook(&session, agent, &payload) {
                        self.abandon(&error, commands.by_ref());
                        return;
                    }
                    // Core decides what follows from the frames, e.g. naming the session after its first prompt.
                    follow_up.push_back(HostEvent::HookTranslated { session, turn_end });
                }
                HostCommand::Capture { captures, then } => {
                    self.capture(captures, then, follow_up);
                }
                // Nor does a PTY spawn, e.g. for a create that finished just as the shutdown cancelled it: the PTYs
                // were killed when it began, so nothing would stop this one. Its session stops with the host.
                HostCommand::SpawnPty { session, .. } if self.shutdown_by.is_some() => {
                    tracing::debug!(%session, "not spawning: the host is shutting down");
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
                HostCommand::Peers(command) => self.call_peers(command),
                HostCommand::CheckPr {
                    session,
                    repo,
                    branch,
                } => {
                    let failed = |why: String| PrEvent::Checked {
                        session: session.clone(),
                        result: Err(ForgeError::Other(why)),
                    };
                    let panicked =
                        failed(format!("asking about {branch}'s PR failed unexpectedly"));
                    let call = move |forge: &F| PrEvent::Checked {
                        result: forge.pr_for_branch(&repo, &branch),
                        session,
                    };
                    // Counted either way: an unanswered call is answered through the channel, as a failure.
                    self.forge_calls += 1;
                    if let Err(unanswered) = self.forge.submit(call, panicked) {
                        tracing::error!("the forge thread is gone");
                        self.tx.send(RuntimeEvent::Forge(*unanswered));
                    }
                }
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
                HostCommand::CancelHook { session } => {
                    // Never waits: the hook's own thread waits for its process group to be gone, then answers.
                    if let Some(id) = self.hooks.get(&session) {
                        self.commands.port().cancel(*id);
                    }
                }
                // Once shutdown has started no hook starts: nothing would cancel it, and it would outlive the host. Its
                // session stops with the host, as a hook's answer during shutdown would be dropped anyway.
                HostCommand::RunHook { .. } | HostCommand::RunPreRemove { .. }
                    if self.shutdown_by.is_some() =>
                {
                    tracing::debug!("not starting a hook: the host is shutting down");
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
                    let (id, thread) = self.next_hook(&session);
                    let running = session.clone();
                    let call = move |commands: &C| HostEvent::HookFinished {
                        result: commands.run(id, &command),
                        session,
                        command,
                        rest,
                        spawn,
                    };
                    // On a thread of its own, so it holds up no other session's hooks.
                    match self.commands.run_alone(&thread, call, panicked) {
                        Ok(()) => {
                            self.hooks.insert(running, id);
                        }
                        Err(unanswered) => {
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
                    remover: Remover::Session(ref session),
                    purpose: Inspect::Close { .. },
                    ..
                } if self.hooks.contains_key(session) => {
                    self.after_hook.insert(session.clone(), command);
                }
                HostCommand::InspectWorktree {
                    remover,
                    repo,
                    path,
                    purpose,
                } => {
                    let panicked = HostEvent::WorktreeInspected {
                        remover: remover.clone(),
                        path: path.clone(),
                        purpose: purpose.clone(),
                        result: Err(WorktreeError::Other(format!(
                            "inspecting worktree {path} failed unexpectedly"
                        ))),
                    };
                    let call = move |worktrees: &W| HostEvent::WorktreeInspected {
                        result: inspect_worktree(worktrees, &repo, &path),
                        remover,
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
                HostCommand::ListWorktrees { reply, repo } => {
                    let panicked = HostEvent::WorktreesListed {
                        reply,
                        result: Err(WorktreeError::Other(format!(
                            "listing the worktrees of {repo} failed unexpectedly"
                        ))),
                    };
                    let call = move |worktrees: &W| HostEvent::WorktreesListed {
                        reply,
                        result: worktrees.list_managed(&repo),
                    };
                    match self.worktrees.submit(call, panicked) {
                        Ok(()) => self.worktree_calls += 1,
                        Err(unanswered) => {
                            tracing::error!("the worktrees thread is gone");
                            follow_up.push_back(*unanswered);
                        }
                    }
                }
                HostCommand::InspectOrphans {
                    reply,
                    repo,
                    skip,
                    after_days,
                } => {
                    let panicked = HostEvent::OrphansInspected {
                        reply,
                        after_days,
                        result: Err(WorktreeError::Other(format!(
                            "inspecting the worktrees of {repo} failed unexpectedly"
                        ))),
                    };
                    let call = move |worktrees: &W| HostEvent::OrphansInspected {
                        reply,
                        after_days,
                        result: inspect_orphans(worktrees, &repo, &skip),
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
                    remover,
                    command,
                    rest,
                    path,
                    force,
                } => {
                    let panicked = HostEvent::PreRemoveFinished {
                        remover: remover.clone(),
                        command: command.clone(),
                        rest: rest.clone(),
                        path: path.clone(),
                        force,
                        result: Err(CommandError::Failed(
                            "running it failed unexpectedly".into(),
                        )),
                    };
                    let id = self.commands.port().new_run();
                    let (thread, running) = match &remover {
                        Remover::Session(session) => {
                            (format!("hook-{session}"), Remover::Session(session.clone()))
                        }
                        Remover::Orphan { reply, .. } => {
                            (format!("pre-remove-{}", reply.get()), remover.clone())
                        }
                    };
                    let call = move |commands: &C| HostEvent::PreRemoveFinished {
                        result: commands.run(id, &command),
                        remover,
                        command,
                        rest,
                        path,
                        force,
                    };
                    match self.commands.run_alone(&thread, call, panicked) {
                        Ok(()) => match running {
                            Remover::Session(session) => {
                                self.hooks.insert(session, id);
                            }
                            Remover::Orphan { reply, .. } => {
                                self.cleanup_hooks.insert(reply, id);
                            }
                        },
                        Err(unanswered) => follow_up.push_back(*unanswered),
                    }
                }
                HostCommand::RemoveWorktree {
                    remover,
                    path,
                    force,
                    note,
                } => {
                    let panicked = HostEvent::WorktreeRemoved {
                        remover: remover.clone(),
                        path: path.clone(),
                        result: Err(WorktreeError::Other(format!(
                            "removing worktree {path} failed unexpectedly"
                        ))),
                        exists: true,
                        note: note.clone(),
                    };
                    let call = move |worktrees: &W| {
                        let result = worktrees.remove(&path, force);
                        HostEvent::WorktreeRemoved {
                            exists: anything_at(&path),
                            result,
                            remover,
                            path,
                            note,
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

    /// Submits `captures` to the archive's worker, stamped with the time now, its answer coming back as
    /// [`HostEvent::Captured`] with `then`. A turn end's capture is dropped while another of that session's waits to
    /// start: the waiting one will read the transcript as it is by then.
    fn capture(
        &mut self,
        captures: Vec<CaptureJob>,
        then: AfterCapture,
        follow_up: &mut VecDeque<HostEvent>,
    ) {
        let turn_end = match (&then, captures.as_slice()) {
            (AfterCapture::Nothing, [job]) if job.reason == CaptureReason::TurnEnd => {
                Some((job.session.clone(), job.agent_session_id.clone()))
            }
            _ => None,
        };
        if let Some(key @ (session, _)) = &turn_end
            && !lock(&self.queued_turn_ends).insert(key.clone())
        {
            tracing::debug!(%session, "a capture of this agent session is already waiting");
            return;
        }
        let at = self.env.now().instant();
        let this = self.machine.clone();
        // Each agent's home as its session was launched: its profile's env, as the config says now.
        let homes: Vec<Option<PathBuf>> = captures.iter().map(|job| self.agent_home(job)).collect();
        let worktrees = self.worktrees.shared();
        let queued = self.queued_turn_ends.clone();
        let removing = matches!(then, AfterCapture::Remove { .. });
        let panicked = Archived {
            event: HostEvent::Captured {
                then: then.clone(),
                result: Err(ArchiveError::Failed(
                    "archiving the conversation failed unexpectedly".into(),
                )),
            },
            outside_repo: Vec::new(),
        };
        let waiting = turn_end.clone();
        let call = move |archive: &A| {
            if let Some(key) = &waiting {
                lock(&queued).remove(key);
            }
            let mut outside_repo = Vec::new();
            let jobs = captures.iter().zip(&homes);
            let result = capture_all(
                archive,
                &*worktrees,
                jobs,
                at,
                &this,
                removing,
                &mut outside_repo,
            );
            Archived {
                event: HostEvent::Captured { result, then },
                outside_repo,
            }
        };
        match self.archive.submit(call, panicked) {
            Ok(()) => self.capture_calls += 1,
            Err(unanswered) => {
                tracing::error!("the archive thread is gone");
                if let Some(key) = &turn_end {
                    lock(&self.queued_turn_ends).remove(key);
                }
                follow_up.push_back(unanswered.event);
            }
        }
    }

    /// Where `job`'s agent keeps its transcripts: under the home its profile's env gives it (the env it was launched
    /// with), else the host's. A session whose profile isn't recorded, or is no longer in the config, gets the host's.
    fn agent_home(&self, job: &CaptureJob) -> Option<PathBuf> {
        let none = BTreeMap::new();
        let env = job
            .profile
            .as_deref()
            .and_then(|name| self.settings.config.profile(name))
            .map_or(&none, |profile| &profile.env);
        self.settings
            .agent_homes
            .launched_with(job.agent, env, job.cwd.as_path())
    }

    /// A new id for a hook of `session`, and the name of the thread it runs on.
    fn next_hook(&self, session: &SessionId) -> (RunId, String) {
        (self.commands.port().new_run(), format!("hook-{session}"))
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
        self.pr.apply(&envelope);
        self.schedule_pr(envelope.ts());
        self.peering.apply(&envelope);
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
        if self.pr_due.is_some_and(|due| due <= now) && self.shutdown_by.is_none() {
            self.tick_pr();
        }
        if self.peer_due.is_some_and(|due| due <= now) && self.shutdown_by.is_none() {
            let at = self.env.now();
            self.peering(PeeringEvent::Tick { at });
        }
        if self.tunnel_due.is_some_and(|due| due <= now) {
            self.tunnel_due = None;
            let ends: Vec<ConnId> = self
                .tunnels
                .iter()
                .filter(|(_, end)| end.paused || !end.held.is_empty())
                .map(|(&conn, _)| conn)
                .collect();
            let mut progress = false;
            for conn in ends {
                progress |= self.tunnel_flow(conn);
            }
            if self.tunnels.values().any(|end| end.paused) {
                self.tunnel_wait = match progress {
                    true => TUNNEL_TICK,
                    false => (self.tunnel_wait * 2).min(MAX_TUNNEL_TICK),
                };
                self.tunnel_due = Some(now + self.tunnel_wait);
            }
        }
        if self.shutdown_by.is_some_and(|by| by <= now) && !self.finished {
            tracing::warn!(
                ptys = self.screens.len(),
                hooks = self.hooks.len(),
                cleanup_hooks = self.cleanup_hooks.len(),
                creates = self.creating.len(),
                captures = self.capture_calls,
                "not everything stopped in time; stopping anyway"
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

    /// Asks the forge about every session whose check is due, and schedules the PR watch's next deadline.
    fn tick_pr(&mut self) {
        let at = self.env.now();
        self.pr_ticked = Some((at.clone(), self.env.instant()));
        let commands = self.pr.handle(PrEvent::Tick { at: at.clone() });
        self.execute_all(commands);
        self.schedule_pr(&at);
    }

    /// Puts the PR watch's deadline on the monotonic clock, `at` being the wall-clock time now.
    fn schedule_pr(&mut self, at: &Timestamp) {
        self.pr_due = self
            .pr
            .deadline()
            .map(|deadline| self.env.instant() + until(at, deadline));
    }

    /// Hands `event` to the peering and executes what it decides, then what that batch changed (`Peering::sync`), and
    /// schedules its deadline.
    fn peering(&mut self, event: PeeringEvent) {
        let commands = self.peering.handle(event);
        self.execute_all(commands);
        let sync = self.peering.sync();
        self.execute_all(sync);
        let at = self.env.now();
        self.peer_due = self
            .peering
            .deadline()
            .map(|deadline| self.env.instant() + until(&at, deadline));
    }

    /// One call to the `Peers` port, which never blocks.
    fn call_peers(&mut self, command: PeersCommand) {
        match command {
            PeersCommand::Bind => self.peers.bind(),
            PeersCommand::Unbind => self.peers.unbind(),
            PeersCommand::SetTrusted(trusted) => self.peers.set_trusted(&trusted),
            PeersCommand::AcceptPairing { id, token, name } => {
                self.peers.accept_pairing(id, token, &name)
            }
            PeersCommand::StopPairing => self.peers.stop_pairing(),
            PeersCommand::Join { id, ticket, name } => self.peers.join(id, &ticket, &name),
            PeersCommand::Connect { peer } => self.peers.connect(&peer),
        }
    }

    /// Executes `commands` from a watch beside core, then the events that produced for core.
    fn execute_all(&mut self, commands: Vec<HostCommand>) {
        let mut follow_up = VecDeque::new();
        self.execute(commands, &mut follow_up);
        for event in follow_up {
            self.dispatch(event);
        }
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
        let first = self.conns.get_mut(&conn).is_some_and(|c| {
            let first = !c.spoke;
            c.spoke = true;
            first
        });
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
                    size: screen_size(resize.cols, resize.rows),
                }),
            },
            // Once the PTYs are exported only hook frames are taken, so nothing is started that the swap can't carry.
            Ok(ClientMessage::Request(request))
                if self.swapping && !matches!(request.body, ClientRequest::HookFrame(_)) =>
            {
                tracing::debug!(%conn, "dropped a request during the swap");
            }
            Ok(ClientMessage::Request(request)) => {
                self.client_request(conn, request.id, request.body, first)
            }
        }
    }

    /// Handles one request; `first` says it is the first line `conn` sent.
    fn client_request(&mut self, conn: ConnId, id: RequestId, body: ClientRequest, first: bool) {
        if let Some(refusal) = peering::refused_to_peers(&body) {
            match self.link.peer(conn) {
                Ok(None) => {}
                Ok(Some(machine)) => {
                    tracing::warn!(%conn, %machine, "refused a request only this machine's clients may make");
                    let result = Err(refusal);
                    return self.respond(conn, &HostResponse { id, result });
                }
                // A conn the link no longer knows may have been anyone's: refused, never taken for a local one.
                Err(error) => {
                    tracing::debug!(%conn, %error, "a local-only request on a conn that is gone");
                    return self.forget(conn);
                }
            }
        }
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
                    let ok = ok.with_warning(settings.warning.clone());
                    let ok = ok.with_machine_name(self.host.machine_name().map(str::to_owned));
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
            ClientRequest::WorktreeList(_) => {
                let reply = self.reply_to(conn, id);
                self.dispatch(HostEvent::ListWorktrees { reply });
            }
            ClientRequest::WorktreeCleanup(_) => {
                let reply = self.reply_to(conn, id);
                let after_days = self.settings.config.cleanup_after_days();
                self.dispatch(HostEvent::ListCleanup { reply, after_days });
            }
            ClientRequest::WorktreeRemove(remove) => {
                let reply = self.reply_to(conn, id);
                // The hooks of the current workspace, whose worktree it is, as the config says now.
                let hooks = match self.host.workspace() {
                    Some(workspace) => self.settings.config.workspace_hooks(workspace),
                    None => WorkspaceHooks::default(),
                };
                self.dispatch(HostEvent::RemoveOrphan {
                    reply,
                    path: PathBuf::from(remove.path),
                    hooks,
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
            ClientRequest::PairStart(_) => {
                let reply = self.reply_to(conn, id);
                let token = match self.env.pair_token() {
                    Ok(token) => token,
                    Err(why) => {
                        let error =
                            WireError::Internal(format!("cannot make a pairing token: {why}"));
                        return self.reply(reply, Err(error));
                    }
                };
                let at = self.env.now();
                self.peering(PeeringEvent::Start { reply, token, at });
            }
            ClientRequest::PairWait(_) => {
                let reply = self.reply_to(conn, id);
                self.peering(PeeringEvent::Wait { reply });
            }
            ClientRequest::PairJoin(join) => {
                let reply = self.reply_to(conn, id);
                let ticket = PairTicket::new(join.ticket);
                self.peering(PeeringEvent::Join { reply, ticket });
            }
            ClientRequest::PeerList(_) => {
                let reply = self.reply_to(conn, id);
                self.peering(PeeringEvent::List { reply });
            }
            ClientRequest::PeerRemove(remove) => {
                let reply = self.reply_to(conn, id);
                self.peering(PeeringEvent::Remove {
                    reply,
                    peer: remove.peer,
                });
            }
            ClientRequest::Tunnel(tunnel) => self.tunnel(conn, id, tunnel.machine, first),
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
                port_calls: self.worktree_calls
                    + self.hooks.len()
                    + self.cleanup_hooks.len()
                    + self.capture_calls,
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
    /// `[defaults] agent`, in the requested directory, else the workspace's root, else the home directory, at the
    /// client's view size, else [`INITIAL_SIZE`] (a resize before it spawns wins, see [`Sizes`]).
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
            // Resolved, so core compares one spelling of each directory: `..` and symlinks can't make a worktree in
            // use look free.
            (Some(cwd), _) => canonical(&existing_dir(Path::new(cwd))?)?,
            // Checked here too, to say which workspace is gone; the spawn checks every cwd again.
            (None, Some(workspace)) => existing_dir(workspace).map_err(|_| {
                WireError::InvalidParams(format!(
                    "the workspace {} no longer exists; choose another",
                    workspace.display()
                ))
            })?,
            (None, None) => self.settings.home.clone(),
        };
        let size = new
            .size
            .map_or(INITIAL_SIZE, |s| screen_size(s.cols, s.rows));
        let ctx = LaunchContext {
            session: session.clone(),
            cwd,
            size,
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
            worktree_branch: new.branch.clone().filter(|_| new.cwd.is_some()),
            spawn: launch.spawn,
            hooks,
        };
        Ok((spec, launch.files))
    }

    /// What resuming `session` runs, and the run files to write before spawning it: its profile, as it is in the
    /// config now, planned with the agent session to resume in the directory it started in (see
    /// [`Host::resume_target`]), which must still be a directory, at [`INITIAL_SIZE`] unless a size is known for it
    /// ([`Sizes`]). Core checks the profile is still the kind the session started with.
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

    /// `conn`'s catch-up found `queued` bytes waiting. Schedules the next check, backing off while the link delivers
    /// none of it, and closes the conn once it has delivered nothing for [`STALL_TIMEOUT`].
    fn block(&mut self, conn: ConnId, queued: usize) {
        let now = self.env.instant();
        let Some(drained) = self.link.drained_bytes(conn) else {
            self.forget(conn);
            return;
        };
        let Some(sub) = self.conns.get_mut(&conn).and_then(|c| c.sub.as_mut()) else {
            return;
        };
        let blocked = match sub.blocked {
            Some(b) if drained <= b.drained => {
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
                    retry_at: now + wait,
                    wait,
                    ..b
                }
            }
            // Newly blocked, or some of the queue was delivered since the last check: progress.
            _ => Blocked {
                drained,
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
        let gone: Vec<ReplyTo> = self
            .replies
            .iter()
            .filter(|(_, (c, _))| *c == conn)
            .map(|(reply, _)| *reply)
            .collect();
        self.replies.retain(|_, (c, _)| *c != conn);
        // A pairing nobody waits on any more (`shepherd pair` stopped with Ctrl-C) is stopped.
        for reply in gone {
            self.peering(PeeringEvent::Gone { reply });
        }
    }

    // ---------- tunnels ----------

    /// `_shepherd/tunnel` to `machine` as request `id` on `conn`: opens a conn to that machine's host and splices the
    /// two, or answers why not. A refused tunnel from one of this machine's clients, as its first request, closes the
    /// conn; any other is only answered.
    fn tunnel(&mut self, conn: ConnId, id: RequestId, machine: MachineId, first: bool) {
        // A conn the link no longer knows may have been anyone's: refused, never taken for a local one.
        let from = match self.link.peer(conn) {
            Ok(from) => from,
            Err(error) => {
                tracing::debug!(%conn, %error, "tunnel asked for on a conn that is gone");
                return self.forget(conn);
            }
        };
        let open = self
            .tunnels
            .values()
            .filter(|end| end.to.as_ref() == Some(&machine))
            .count();
        if let Err(error) = self.peering.tunnel(&machine, from.as_ref(), first, open) {
            let close = from.is_none() && first;
            return self.refuse_tunnel(conn, id, error, close);
        }
        let other = match self.link.open(&machine) {
            Ok(other) => other,
            Err(error) => {
                tracing::warn!(%machine, %error, "cannot open a tunnel");
                let error = self
                    .peering
                    .tunnel_failed(&machine, &DisconnectReason::PeerGone);
                return self.refuse_tunnel(conn, id, error, true);
            }
        };
        tracing::info!(%conn, %machine, "opening a tunnel");
        // Not a client of this host any more: from now on its lines are the other host's.
        self.conns.remove(&conn);
        let mut client = TunnelEnd::new(other);
        client.opening = Some((id, machine.clone()));
        self.tunnels.insert(conn, client);
        let mut far = TunnelEnd::new(conn);
        far.to = Some(machine);
        self.tunnels.insert(other, far);
    }

    /// Answers tunnel request `id` on `conn` with `error`, then closes `conn` if `close`.
    fn refuse_tunnel(&mut self, conn: ConnId, id: RequestId, error: WireError, close: bool) {
        self.respond(
            conn,
            &HostResponse {
                id,
                result: Err(error),
            },
        );
        if close {
            self.forget(conn);
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
    }

    /// The other host accepted the tunnel conn `conn`: the client's request is answered.
    fn tunnel_open(&mut self, conn: ConnId) {
        let Some(client) = self.tunnels.get(&conn).map(|end| end.other) else {
            return;
        };
        let Some((id, machine)) = self
            .tunnels
            .get_mut(&client)
            .and_then(|end| end.opening.take())
        else {
            return;
        };
        tracing::info!(conn = %client, %machine, "tunnel open");
        self.respond(
            client,
            &HostResponse {
                id,
                result: Ok(HostResult::Ack),
            },
        );
    }

    /// A line read from tunnel end `conn`: sent on to the other end if it has room, else held. Holding more than
    /// [`TUNNEL_HELD_MAX`] closes the tunnel.
    fn tunnel_line(&mut self, conn: ConnId, line: Vec<u8>) {
        let Some(end) = self.tunnels.get_mut(&conn) else {
            return;
        };
        end.held_bytes += line.len();
        end.held.push_back(line);
        self.tunnel_flow(conn);
        let held = self.tunnels.get(&conn).map_or(0, |end| end.held_bytes);
        if held > TUNNEL_HELD_MAX {
            return self.tunnel_overflow(conn, held);
        }
        if self.tunnel_due.is_none() && self.tunnels.get(&conn).is_some_and(|end| end.paused) {
            self.tunnel_wait = TUNNEL_TICK;
            self.tunnel_due = Some(self.env.instant() + TUNNEL_TICK);
        }
    }

    /// Sends what tunnel end `conn` holds while the other end has room, then stops or resumes reading `conn`: stopped
    /// while it holds lines or the other end's queue is at [`TUNNEL_HIGH`], read again once neither is so and the queue
    /// is under [`TUNNEL_LOW`]. Returns whether it made progress: sent a line, found the other end's queue shorter than
    /// last time, or resumed reading.
    fn tunnel_flow(&mut self, conn: ConnId) -> bool {
        let Some(other) = self.tunnels.get(&conn).map(|end| end.other) else {
            return false;
        };
        let mut progress = false;
        loop {
            let queued = self.link.queued_bytes(other).unwrap_or(0);
            let Some(end) = self.tunnels.get_mut(&conn) else {
                return progress;
            };
            if queued < end.last_queued {
                progress = true;
            }
            end.last_queued = queued;
            let room = |len: usize| queued == 0 || queued + len <= TUNNEL_HIGH;
            match end.held.front() {
                Some(line) if room(line.len()) => {
                    let line = end.held.pop_front().unwrap_or_default();
                    end.held_bytes = end.held_bytes.saturating_sub(line.len());
                    self.send(other, &line);
                    progress = true;
                }
                _ => break,
            }
        }
        let queued = self.link.queued_bytes(other).unwrap_or(0);
        let Some(end) = self.tunnels.get_mut(&conn) else {
            return progress;
        };
        end.last_queued = queued;
        let pause = !end.held.is_empty() || queued >= TUNNEL_HIGH;
        let resume = end.held.is_empty() && queued < TUNNEL_LOW;
        let change = match end.paused {
            false if pause => Some(true),
            true if resume => Some(false),
            _ => None,
        };
        if let Some(paused) = change {
            end.paused = paused;
            progress |= !paused;
            if let Err(error) = self.link.pause_reading(conn, paused) {
                tracing::debug!(%conn, %error, "pause failed");
            }
        }
        progress
    }

    /// Tunnel end `conn` holds `held` bytes, more than [`TUNNEL_HELD_MAX`], that its other end has no room for: that end
    /// is a slow consumer. Both ends are closed, what was held dropped (the other end has what its queue holds).
    fn tunnel_overflow(&mut self, conn: ConnId, held: usize) {
        let Some(end) = self.tunnels.remove(&conn) else {
            return;
        };
        let other = end.other;
        let other_end = self.tunnels.remove(&other);
        tracing::warn!(
            %conn,
            conn_other = %other,
            held,
            "closing a tunnel: its other end read too slowly, and more than {TUNNEL_HELD_MAX} bytes waited for it"
        );
        // A client whose tunnel hadn't opened yet is told why, as when it can't open.
        let opening = [
            (conn, end.opening),
            (other, other_end.and_then(|e| e.opening)),
        ];
        for (client, opening) in opening {
            if let Some((id, machine)) = opening {
                let error = self
                    .peering
                    .tunnel_failed(&machine, &DisconnectReason::SlowConsumer);
                self.respond(
                    client,
                    &HostResponse {
                        id,
                        result: Err(error),
                    },
                );
            }
        }
        for conn in [conn, other] {
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
    }

    /// Tunnel end `conn` is gone, for `reason`: the other end gets what `conn` held, then is closed. A client whose
    /// tunnel never opened is told why first.
    fn tunnel_gone(&mut self, conn: ConnId, reason: &DisconnectReason) {
        let Some(end) = self.tunnels.remove(&conn) else {
            return;
        };
        let other = end.other;
        let opening = self.tunnels.remove(&other).and_then(|o| o.opening);
        match opening {
            Some((id, machine)) => {
                tracing::info!(conn = %other, %machine, ?reason, "tunnel didn't open");
                let error = self.peering.tunnel_failed(&machine, reason);
                self.respond(
                    other,
                    &HostResponse {
                        id,
                        result: Err(error),
                    },
                );
            }
            None => {
                tracing::info!(%conn, conn_other = %other, ?reason, "tunnel closed");
                for line in end.held {
                    self.send(other, &line);
                }
            }
        }
        if let Err(error) = self.link.close(other) {
            tracing::debug!(conn = %other, %error, "close failed");
        }
    }

    // ---------- shutdown ----------

    fn start_shutdown(&mut self) {
        if self.shutdown_by.is_some() {
            return;
        }
        // A capture reads transcripts and writes the archive: it is waited for as long as a create's cleanup.
        let grace = match self.creating.is_empty() && self.capture_calls == 0 {
            true => SHUTDOWN_GRACE,
            false => CREATE_CLEANUP_GRACE,
        };
        self.shutdown_by = Some(self.env.instant() + grace);
        // A create has no time cap: one left running would outlive the host. The shutdown waits for its answer, once it
        // has cleaned up, and core records it (a worktree it made all the same is recorded, for cleanup).
        for (repo, name) in self.creating.values() {
            self.worktrees.port().cancel_create(repo, name);
        }
        // Hooks run in sessions of their own, so one left running would outlive the host too. Each answers once its
        // process group is gone, and the shutdown waits for that.
        for id in self.hooks.values().chain(self.cleanup_hooks.values()) {
            self.commands.port().cancel(*id);
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
        if self.shutdown_by.is_some()
            && self.screens.is_empty()
            && self.hooks.is_empty()
            && self.cleanup_hooks.is_empty()
            && self.creating.is_empty()
            && self.capture_calls == 0
        {
            self.finish();
        }
    }

    fn finish(&mut self) {
        let conns: Vec<ConnId> = self
            .conns
            .keys()
            .chain(self.tunnels.keys())
            .copied()
            .collect();
        self.tunnels.clear();
        self.tunnel_due = None;
        for conn in conns {
            if let Err(error) = self.link.close(conn) {
                tracing::debug!(%conn, %error, "close failed");
            }
        }
        self.conns.clear();
        self.replies.clear();
        // Peers see this machine go at once, rather than when its connections time out.
        self.peers.unbind();
        self.finished = true;
    }
}

/// Logs what the `Peers` port reports that nothing else shows: refusals, failed dials and binds, connections.
fn log_peers_event(event: &PeersEvent) {
    match event {
        PeersEvent::Refused { peer } => {
            tracing::warn!(%peer, "refused a connection from a machine this one doesn't trust")
        }
        PeersEvent::BindFailed { error } => tracing::error!(%error, "cannot go online"),
        PeersEvent::ConnectFailed { peer, error } => {
            tracing::info!(%peer, %error, "cannot connect to a peer")
        }
        PeersEvent::Connected { peer } => tracing::info!(%peer, "peer connected"),
        PeersEvent::Disconnected { peer } => tracing::info!(%peer, "peer disconnected"),
        _ => {}
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

/// Whether `path` is one of `repo`'s managed worktrees, at exactly the path the port lists, and if so its status and branch; and
/// whether anything is there at all (without following a symlink). Runs on the worker thread.
fn inspect_worktree(
    worktrees: &impl Worktrees,
    repo: &RepoRef,
    path: &WorktreePath,
) -> Result<Inspection, WorktreeError> {
    let listed = worktrees.list_managed(repo)?;
    if let Some(found) = listed.into_iter().find(|w| w.path == *path) {
        let status = worktrees.status(path);
        let exists = anything_at(path);
        let branch = found.branch.map(|b| b.as_str().to_owned());
        return Ok(Inspection::Listed {
            status,
            exists,
            branch,
        });
    }
    let exists = anything_at(path);
    Ok(Inspection::Unlisted { exists })
}

/// `repo`'s managed worktrees that aren't in `skip`, each with its status. Runs on the worker thread.
fn inspect_orphans(
    worktrees: &impl Worktrees,
    repo: &RepoRef,
    skip: &[PathBuf],
) -> Result<InspectedOrphans, WorktreeError> {
    let listed = worktrees.list_managed(repo)?;
    Ok(listed
        .into_iter()
        .filter(|w| !skip.iter().any(|s| s == w.path.as_path()))
        .map(|w| {
            let status = worktrees.status(&w.path);
            (w, status)
        })
        .collect())
}

/// Takes each of `jobs` (with its agent's home) in order, stopping at the first that fails: finds its repo's root commit
/// and its commits since its base (`Worktrees`), then records its session and snapshots its transcript (`Archive`). A
/// job whose cwd is in no repo is skipped and added to `outside_repo`, unless `removing` (then it fails). Runs on the
/// archive's worker thread.
fn capture_all<'a>(
    archive: &impl Archive,
    worktrees: &impl Worktrees,
    jobs: impl IntoIterator<Item = (&'a CaptureJob, &'a Option<PathBuf>)>,
    at: jiff::Timestamp,
    this: &MachineId,
    removing: bool,
    outside_repo: &mut Vec<(SessionId, WorktreePath)>,
) -> Result<(), ArchiveError> {
    for (job, home) in jobs {
        let home = home.as_ref().ok_or_else(|| {
            ArchiveError::Refused(format!(
                "{} sessions have no transcript",
                job.agent.as_str()
            ))
        })?;
        let repo = match worktrees.root_commit(&RepoRef::new(job.cwd.as_path())) {
            Ok(repo) => repo,
            // Nothing to file it under. Only a removal waits on a capture, and it keeps its worktree on a failure.
            Err(WorktreeError::NotARepo(_)) if !removing => {
                outside_repo.push((job.session.clone(), job.cwd.clone()));
                continue;
            }
            Err(e) => {
                return Err(ArchiveError::Failed(format!(
                    "finding the repo at {}: {e}",
                    job.cwd
                )));
            }
        };
        let commits = match &job.base {
            Some(base) => worktrees.commits_since(&job.cwd, base).map_err(|e| {
                ArchiveError::Failed(format!("listing the commits in {}: {e}", job.cwd))
            })?,
            None => Vec::new(),
        };
        let found = Found { repo, commits };
        let (session, request) = job.requests(found, at, this, home.clone());
        archive.put_session(&session)?;
        archive.capture(&request)?;
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
/// `dir`, an existing directory, with `..` and symlinks resolved.
fn canonical(dir: &Path) -> Result<PathBuf, WireError> {
    std::fs::canonicalize(dir)
        .map_err(|e| WireError::InvalidParams(format!("cwd `{}`: {e}", dir.display())))
}

/// How long from `at` until `deadline`: zero if it has passed.
fn until(at: &Timestamp, deadline: jiff::Timestamp) -> Duration {
    Duration::try_from(deadline.duration_since(at.instant())).unwrap_or(Duration::ZERO)
}

/// A size a client sent, clamped to what a [`Screen`] holds, so the host's screen and the PTY always agree.
fn screen_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(MIN_COLS, MAX_COLS),
        rows: rows.clamp(MIN_ROWS, MAX_ROWS),
    }
}

/// `envelope` as core folds it at start: a `session/created` or `worktree/created` recorded before paths were resolved
/// (a worktrees root or cwd behind a symlink) with its paths resolved, as a new one would record them, so core
/// compares one spelling. A path that no longer resolves is kept as recorded. The log itself is never rewritten.
fn resolved_paths(envelope: &Envelope) -> Envelope {
    let resolve = |path: &str| {
        std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| path.to_owned())
    };
    let Frame::Shepherd(message) = envelope.frame() else {
        return envelope.clone();
    };
    let frame = match message.body() {
        ShepherdFrame::SessionCreated(created) => {
            let mut created = created.clone();
            created.worktree = created.worktree.as_deref().map(resolve);
            created.cwd = created.cwd.as_deref().map(resolve);
            ShepherdFrame::SessionCreated(created)
        }
        ShepherdFrame::WorktreeCreated(made) => {
            let mut made = made.clone();
            made.path = resolve(&made.path);
            ShepherdFrame::WorktreeCreated(made)
        }
        _ => return envelope.clone(),
    };
    let draft = FrameDraft::new(
        frame.to_frame(),
        envelope.source().clone(),
        envelope.dir().clone(),
        envelope.session().cloned(),
    );
    let stamp = Stamp {
        id: envelope.id().clone(),
        ts: envelope.ts().clone(),
        machine: envelope.machine().clone(),
    };
    draft.seal(envelope.seq(), stamp)
}

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
