//! The client's view state and decisions: connecting to a host, the merged session list, selection, focus and
//! popups, and turning what the user means into wire messages.
//!
//! Attention: a session wants the user when it starts waiting on them (asks permission or input, or goes idle) at
//! a seq of its log the user hasn't seen. While the terminal window has focus, the selected session is seen up to its
//! latest entry, so it is never marked, and selecting a marked session clears its mark. While the window is in the
//! background ([`Intent::WindowFocus`]) the selected session is treated like any other, and its mark clears when the
//! window regains focus. With the host's `[notify] bell` on, a session that newly wants the user gives a
//! [`ClientCommand::Alert`], except while the log is being caught up after a (re)connect.
//!
//! Workspaces: each machine's host has its own workspace and the ones used before, folded from its log like the
//! sessions. Every session is listed, whatever its workspace; one created in a workspace other than its host's current
//! one is marked ([`ClientSession::elsewhere`]) rather than hidden, so nothing running is ever out of sight. The
//! switcher ([`Popup::Workspace`]) asks a host to select a workspace and stays open until the host answers: a refusal is
//! shown in it.
//!
//! Closing: the close confirmation of a session with a worktree asks the host what removing the worktree would lose
//! ([`Client::close_check`]). If it loses nothing, confirming lets the host remove it (if that is still true when it
//! closes); if it would lose work, the confirmation lists what and offers to remove it anyway or keep it, and confirming
//! keeps it. A session is listed while its worktree is being removed, and can't be closed again. When the host keeps a
//! worktree it tried to remove, the reason is a notice, and otherwise so is any note on how closing went, e.g. that no
//! transcript was found to archive (live entries only, not the catch-up).
//!
//! Builds (spike T36): the hello carries this client's [`BuildId`], and the answer the host's and what is installed at
//! its path. A stale host (the installed build is this client's) is asked to update itself in place
//! (`_shepherd/host/upgrade`, T36b): the footer says so, a busy host is asked again every [`UPGRADE_RETRY`] until
//! [`UPGRADE_DEADLINE`], and once it accepts, the reconnect's hello that finds this build says "host updated". If it
//! can't, it falls back to the restart dialog. A host older than build identities, one speaking another protocol
//! version, or a stale one that can't update itself gets the restart dialog ([`Popup::RestartHost`]), which says how many sessions restarting
//! stops; only a click restarts it ([`ClientCommand::TerminateHost`]), and the reconnect then launches this build. A
//! client that is the odd one out only gets a notice. A request the host doesn't know (`-32601`) says the host is older.
//! While the host should be restarted ([`Client::restart_offer`]), the dialog can be reopened ([`Intent::OfferRestart`]);
//! the dialog and the offer go when the link drops, and a hello that finds no reason closes the dialog, so a click
//! can only ever stop the host it was about.
//!
//! Finding (T26): the search field opens the finder ([`Popup::Finder`]), which matches what is typed against every
//! session and the current workspace's orphan worktrees (managed ones no open session runs in), ranked by
//! [`finder::rank`]. Opening it asks the host for the orphans ([`Client::orphans`]); an answer for a finder that has
//! closed since is dropped. Choosing a session selects it; choosing an orphan offers to start a session in it
//! ([`Popup::StartInWorktree`]), which asks for one with that worktree as its cwd and its branch, so the host makes no
//! new worktree and records the branch.
//!
//! Cleanup (T25b): the sidebar's `Cleanup` button opens the cleanup view ([`Popup::Cleanup`]), which asks the host for
//! the workspace's orphan worktrees with their status (`_shepherd/worktree/cleanup`), then for the status of each stale
//! session: one with a worktree in the current workspace whose PTY exited at least the host's `cleanup_after_days`
//! before the latest [`ClientEvent::Tick`], listed only if its branch is merged. Removing the safe rows
//! ([`CleanupRow::safe`]) removes them one at a time, each through the host's own checks: an orphan with
//! `_shepherd/worktree/remove`, a stale session by closing it (its `session/closed` says how that went). Each row says
//! how its removal went. Closing the view stops nothing; when the removals end while it is closed, a notice sums them
//! up. A dropped link leaves the removal under way's outcome unknown (the host most likely finishes it) and unqueues
//! the rest.
//!
//! Machines (T31b): once caught up with its own host, the client asks it for the machines it trusts
//! (`_shepherd/peer/list`) and opens a conn per peer ([`ClientCommand::ConnectPeer`]) whose first request makes it a
//! tunnel to that machine's host (`_shepherd/tunnel`, decision 0019); from then on it speaks to that host as to its
//! own: hello (without this machine's cwd, which means nothing there), then a subscribe. Each host has its own requests,
//! fold and seqs ([`ClientEvent::Peer`]). Sessions are listed grouped by the host they came from, this machine's
//! first, then the peers by name, each newest first ([`Client::sidebar`] adds a header per machine while there are
//! peers). A session id two hosts both claim is listed once, as the first group's. Input, resizes, renames, closes,
//! resumes and the close check go to the session's own host. A peer that
//! drops keeps its last-known sessions (shown dimmed, [`Client::offline`]) and is dialled again at once; a failed
//! attempt waits [`PEER_RETRY`], doubling up to [`PEER_RETRY_MAX`], counted from the first tick after it failed. A
//! reconnect subscribes from the last entry folded of each of its logs: no gap, no repeat. The finder's orphans, cleanup
//! and host updates stay this machine's host's.
//!
//! Where new sessions run (T32): while there are peers, `+ New session` opens the new-session popup
//! ([`Popup::PickProfile`]) with a machine selector, and the workspace switcher has one too. Each opens on the selected
//! session's machine (this one with nothing selected, or when that machine can't be reached now): see
//! [`Client::popup_machine`]. Only a machine that can be reached now can be chosen ([`Intent::PickMachine`]). The
//! profiles offered, the workspaces listed, and where the session starts or the workspace is selected are the chosen
//! machine's host's. Without a popup open, the selected session's machine is the one acted on: the new-session key
//! starts a session there, and [`Client::workspace`] is its workspace.
//!
//! Scripts (T46): a command-line client runs this same core headless. [`Client::settled`] says when every host it can
//! reach has sent its sessions, and [`ClientEvent::Ask`] asks a host something on the caller's behalf: the answer (or
//! why there is none: the machine is unknown or can't be reached, or its link dropped first) is kept for
//! [`Client::answer`], and the client acts on none of it. [`automation`] names sessions and machines.
//!
//! [`Client::handle`] is pure. Request ids are a per-client counter, and time arrives in events
//! ([`ClientEvent::ConnectFailed`], [`ClientEvent::Tick`]).

pub mod automation;
pub mod cleanup;
pub mod finder;

pub use cleanup::{CleanupCheck, CleanupItem, CleanupListing, CleanupOutcome, CleanupRow};
pub use finder::{FinderItem, Orphan, Orphans};

use cleanup::{Cleanup, Removal};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use jiff::SignedDuration;

use crate::acpp::{Envelope, Frame, Pr, RequestId, Seq, ShepherdFrame, Timestamp};
use crate::config::AgentKind;
use crate::host::{Host, ResumeUnavailable};
use crate::ids::{MachineId, SessionId};
use crate::layout::{Content, Layout};
use crate::machine::MachineColor;
use crate::ports::pty::PtySize;
use crate::ports::worktrees::WorktreeStatus;
use crate::session::{Activity, SessionState};
use crate::wire::{
    BuildId, ClientMessage, ClientNotification, ClientRequest, CloseWorktree, Hello, HelloOk,
    HostMessage, HostNotification, HostResult, HostUpgrade, InputKind, Method, PROTOCOL_VERSION,
    PeerInfo, PeerList, ProfileInfo, RawResponse, Request, SessionClose, SessionInput, SessionNew,
    SessionRename, SessionResize, SessionResume, Subscribe, SubscribeFrom, Tunnel, UpgradeAnswer,
    ViewSize, WireError, WorkspaceSelect, WorktreeCheck, WorktreeCleanup, WorktreeList,
    WorktreeRemove,
};

/// Connect attempts per outage, the first included, before the client gives up.
pub const MAX_CONNECT_ATTEMPTS: u32 = 50;
/// How long after a failed connect the next attempt is made.
pub const RETRY_INTERVAL: SignedDuration = SignedDuration::from_millis(100);
/// How long a connected host has to answer the hello before the attempt counts as failed.
pub const HELLO_TIMEOUT: SignedDuration = SignedDuration::from_secs(5);
/// How long a notice shows, from the first tick after it appears, unless a user action clears it first.
pub const NOTICE_TIMEOUT: SignedDuration = SignedDuration::from_secs(5);
/// How soon a host that is too busy to update itself is asked again.
pub const UPGRADE_RETRY: SignedDuration = SignedDuration::from_secs(1);
/// How long a busy host is asked to update itself before the client offers to restart it instead.
pub const UPGRADE_DEADLINE: SignedDuration = SignedDuration::from_secs(60);
/// What a host older than machine names (T29) calls its machine: it is always this one.
pub const UNNAMED_HOST: &str = "local";
/// How long after a failed attempt a peer's host is tried again; it doubles with each failure in a row.
pub const PEER_RETRY: SignedDuration = SignedDuration::from_secs(1);
/// The longest wait between attempts to reach a peer's host.
pub const PEER_RETRY_MAX: SignedDuration = SignedDuration::from_secs(30);

/// Something that happened, for the client to decide on.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The runtime is up: start connecting.
    Start,
    /// A [`ClientCommand::Connect`] succeeded at `at`.
    Connected { at: Timestamp },
    /// A [`ClientCommand::Connect`] failed at `at`.
    ConnectFailed { at: Timestamp, error: String },
    /// The link to the host is gone.
    Disconnected,
    /// A [`ClientCommand::LaunchHost`] failed.
    LaunchFailed { error: String },
    /// A [`ClientCommand::TerminateHost`] failed.
    TerminateFailed { error: String },
    /// Time passes. The runtime sends one regularly, so a waiting retry fires.
    Tick { at: Timestamp },
    /// A message from the host.
    Host(HostMessage),
    /// Something the user wants, already mapped from keys by the TUI.
    User(Intent),
    /// Something on the conn to peer `machine`'s host (a [`ClientCommand::ConnectPeer`]).
    Peer {
        machine: MachineId,
        event: PeerEvent,
    },
    /// Ask the host of `machine` (this machine's for `None`) `request` on behalf of a caller outside the UI, such as a
    /// script: its answer is kept for [`Client::answer`] under `ask`, and nothing else is done with it.
    Ask {
        ask: AskId,
        machine: Option<MachineId>,
        request: ClientRequest,
    },
}

/// What a caller names a [`ClientEvent::Ask`] by, to read its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AskId(pub u64);

/// Why a [`ClientEvent::Ask`] has no result.
#[derive(Debug, Clone, PartialEq)]
pub enum AskError {
    /// No host the client knows of is on this machine.
    UnknownMachine(MachineId),
    /// The machine's host can't be asked now: what it is called, and why, if known.
    NotReachable {
        machine: String,
        why: Option<String>,
    },
    /// The host refused it.
    Refused(WireError),
    /// The link to the machine's host (called `machine`) dropped before it answered: it may or may not have done it.
    Dropped { machine: String },
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownMachine(machine) => write!(f, "no machine {machine} is known"),
            Self::NotReachable { machine, why: None } => write!(f, "{machine} can't be reached"),
            Self::NotReachable {
                machine,
                why: Some(why),
            } => write!(f, "{machine} can't be reached: {why}"),
            Self::Refused(e) => f.write_str(&refusal(e.clone())),
            Self::Dropped { machine } => {
                write!(f, "the connection to {machine} dropped before it answered")
            }
        }
    }
}

impl std::error::Error for AskError {}

/// A [`ClientEvent::Ask`]: waiting for host `at` to answer request `request`, or answered.
#[derive(Debug, Clone)]
enum Ask {
    Pending { at: At, request: RequestId },
    Answered(Result<HostResult, AskError>),
}

/// What happened on the conn to a peer's host.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerEvent {
    /// The [`ClientCommand::ConnectPeer`] opened a conn to the local host at `at`.
    Connected { at: Timestamp },
    /// The [`ClientCommand::ConnectPeer`] failed at `at`.
    ConnectFailed { at: Timestamp, error: String },
    /// The conn is gone.
    Disconnected,
    /// A message on it: the tunnel's answer from the local host, then the peer's host's.
    Host(HostMessage),
}

/// What the user wants, independent of which keys say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    NextSession,
    PrevSession,
    /// Input for the terminal, already encoded for it, and what made it.
    Input(InputKind, Vec<u8>),
    /// The terminal view's size changed.
    Resize(PtySize),
    SetFocus(Focus),
    /// The terminal window shepherd runs in gained (`true`) or lost focus. While it is unfocused the user isn't
    /// looking, so the selected session isn't seen.
    WindowFocus(bool),
    /// Select this session (a click on its row). A session that isn't listed is ignored.
    SelectSession(SessionId),
    /// A click on a machine's header in the sidebar: a peer waiting to be tried again is tried at once; otherwise its
    /// newest session is selected.
    SelectMachine(MachineId),
    /// Scroll the session list by this many rows: down for positive, up for negative. The selection stays put, even
    /// out of view.
    ScrollSidebar(i32),
    /// How many sessions the sidebar shows at once, from the TUI's layout. The list scrolls to fit.
    SidebarRows(usize),
    /// Scroll the open profile picker by `by` rows (down for positive), when it shows `shown` profiles at once, from
    /// the TUI's layout. Ignored when the picker isn't open.
    ScrollPicker {
        by: i32,
        shown: usize,
    },
    /// Scroll the open workspace switcher's list by `by` rows (down for positive), when it shows `shown` workspaces at
    /// once, from the TUI's layout. Ignored when the switcher isn't open.
    ScrollSwitcher {
        by: i32,
        shown: usize,
    },
    /// Scroll the open finder's results by `by` rows (down for positive), when it shows `shown` at once, from the TUI's
    /// layout. Ignored when the finder isn't open.
    ScrollFinder {
        by: i32,
        shown: usize,
    },
    /// Open the finder (a click on the sidebar's search field), and ask the host for the orphan worktrees.
    BeginFind,
    /// Replace the finder's query.
    EditFind(String),
    /// Choose one of the open finder's results: select a session, or offer to start one in an orphan worktree. Ignored
    /// unless it is one of the results.
    FinderChoose(FinderItem),
    /// Start a session with the default profile.
    NewSession,
    /// The `+ New session` button: open the profile picker, or, with at most one profile, start a session at once.
    BeginNewSession,
    /// Start a session with this profile, from the open profile picker. Ignored when the picker isn't open.
    PickProfile(String),
    /// Choose the machine the open new-session popup or workspace switcher acts on (a click on its row). Ignored when
    /// neither is open, or the machine can't be reached now.
    PickMachine(MachineId),
    ShowHelp,
    /// Open the rename popup for the selected session, starting from its current name.
    BeginRename,
    /// Replace the text in the rename popup.
    EditRename(String),
    /// Ask to close the selected session.
    BeginClose,
    /// Ask to close this session (its row's close button), without selecting it. A session that isn't listed is
    /// ignored.
    BeginCloseSession(SessionId),
    /// Open the context menu of this session at a cell of the window (a right click on its row). A session that isn't
    /// listed is ignored.
    OpenMenu {
        session: SessionId,
        at: Cell,
    },
    /// Run an item of the open menu on the menu's session.
    MenuChoose(MenuItem),
    /// Open the workspace switcher (a click on the workspace).
    BeginWorkspace,
    /// Replace the path typed in the workspace switcher.
    EditWorkspacePath(String),
    /// Select this workspace, from the open switcher's list of those used before. Ignored when the switcher isn't open.
    PickWorkspace(PathBuf),
    /// Accept the open popup: send the rename, the close, or the workspace switcher's typed path, choose the finder's
    /// first result, or start the offered session in a worktree. A close whose worktree would lose work keeps the
    /// worktree.
    Confirm,
    /// Close the session of the open close confirmation, removing its worktree even though that loses what the
    /// confirmation showed. Ignored unless it showed something removing would lose.
    CloseRemovingWorktree,
    /// Close the session of the open close confirmation, keeping its worktree.
    CloseKeepingWorktree,
    /// Open the cleanup view (the sidebar's `Cleanup` button), and ask the host what there is to clean, unless removals
    /// are still running, whose rows it shows instead.
    BeginCleanup,
    /// Remove every safe row of the open cleanup view, one at a time. Ignored when it isn't open.
    CleanupRemoveSafe,
    /// Scroll the open cleanup view's rows by `by` (down for positive), when it shows `shown` at once, from the TUI's
    /// layout. Ignored when it isn't open.
    ScrollCleanup {
        by: i32,
        shown: usize,
    },
    /// Close the open popup or menu without doing anything.
    Dismiss,
    /// Reopen the restart dialog, when the host should be restarted ([`Client::restart_offer`]); ignored otherwise.
    OfferRestart,
    /// Restart the host, from the open restart dialog (a click on its `[Restart host]`). Ignored when that isn't open.
    RestartHost,
    /// Show the host's warning again ([`Client::host_warning`]); ignored when it has none.
    ShowHostWarning,
    /// Detach. Sessions keep running on the host.
    Quit,
}

impl Intent {
    /// Keys typed into the terminal, already encoded for it.
    pub fn keys(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Input(InputKind::Keys, bytes.into())
    }
}

/// Something for the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientCommand {
    /// Encode and send this message to the host.
    Send(ClientMessage),
    /// Start the host in the background. Feed [`ClientEvent::LaunchFailed`] if that fails.
    LaunchHost,
    /// Connect to the host. Feed [`ClientEvent::Connected`] or [`ClientEvent::ConnectFailed`].
    Connect,
    /// Stop the running host, so the reconnect launches this client's build. Feed [`ClientEvent::TerminateFailed`]
    /// if that fails. Once it works, the link drops.
    TerminateHost,
    /// Tell the user `session` wants them: the terminal bell and an OSC 9 notification saying `message`.
    Alert { session: SessionId, message: String },
    /// Open a new conn to the local host for peer `machine`'s tunnel. Feed [`PeerEvent::Connected`] or
    /// [`PeerEvent::ConnectFailed`], then its lines and its drop.
    ConnectPeer(MachineId),
    /// Encode and send this message on peer `machine`'s conn.
    SendPeer {
        machine: MachineId,
        message: ClientMessage,
    },
    /// Close peer `machine`'s conn, if it is open. Its drop needn't be fed.
    DisconnectPeer(MachineId),
    /// Stop the client.
    Quit,
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// To the selected session's terminal.
    #[default]
    Terminal,
    /// To shepherd's own commands.
    Command,
}

/// A cell of the window, counted from its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub col: u16,
    pub row: u16,
}

/// What a session's context menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Rename,
    Close,
    /// Resume an exited session's agent where it left off. Offered only on exited sessions.
    Resume,
}

/// One line of the open context menu: [`Client::menu`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuEntry {
    pub item: MenuItem,
    /// Why it can't be chosen, for the user. `None` when it can.
    pub unavailable: Option<ResumeUnavailable>,
}

/// What is open over everything else. It takes the keys and the clicks until it's answered or dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    Help,
    Rename {
        session: SessionId,
        name: String,
    },
    ConfirmClose {
        session: SessionId,
    },
    /// A session's context menu, asked for at `at`. The TUI places it there, kept inside the window.
    Menu {
        session: SessionId,
        at: Cell,
    },
    /// Which profile a new session runs, listing [`Client::profiles`].
    PickProfile,
    /// The workspace switcher: the workspaces used before ([`Client::workspaces`]), and a path to type. `error` is why
    /// the host refused the last path asked for, until the path is edited.
    Workspace {
        path: String,
        error: Option<String>,
    },
    /// The host is out of date and can't update itself: restart it? It says how many sessions that stops
    /// ([`Client::sessions_to_stop`]). Only a click restarts it ([`Intent::RestartHost`]).
    RestartHost {
        why: HostOutdated,
    },
    /// The finder: what has been typed, and the results for it ([`Client::finder`]).
    Finder {
        query: String,
    },
    /// Start a session in this orphan worktree?
    StartInWorktree {
        worktree: Orphan,
    },
    /// The cleanup view: [`Client::cleanup_rows`].
    Cleanup,
}

/// Why the restart dialog offers to restart the host (spike T36's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutdated {
    /// It is older than build identities (its hello answer has no `build`), or speaks another protocol version.
    Older,
    /// It runs an older build than the one now installed at its path, which is this client's.
    Stale,
}

/// What removing the worktree of the session in the open close confirmation would lose, as far as the client knows:
/// [`Client::close_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseCheck {
    /// The host has been asked.
    Checking,
    /// Nothing: closing removes it.
    Safe,
    /// Removing it would lose something ([`WorktreeStatus::losses`]): closing asks whether to remove it anyway, and
    /// removing it anyway tells the host this was what the user saw.
    AtRisk(WorktreeStatus),
    /// It is already gone.
    Gone,
    /// The host couldn't tell, for this reason. Confirming keeps it.
    Failed(String),
}

/// Where the client is with its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// Not started yet.
    Idle,
    /// Connect attempt `attempt` of this outage is under way.
    Connecting {
        attempt: u32,
    },
    /// Connect attempt `attempt` failed. The next one is made at the first tick at or after `until`.
    Waiting {
        attempt: u32,
        until: jiff::Timestamp,
    },
    /// Connect attempt `attempt` reached the host at `since`; waiting for the hello answer. The attempt fails if
    /// the link drops, or no answer comes by [`HELLO_TIMEOUT`] after `since`.
    Handshaking {
        attempt: u32,
        since: jiff::Timestamp,
    },
    Ready,
    /// The host answered the hello with another protocol version: nothing can be asked of it until it is restarted.
    /// The link stays up, and its dropping starts a new outage.
    Incompatible,
    /// The client gave up. The message says why, for the user.
    Failed(String),
}

/// A session as the client lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientSession {
    pub id: SessionId,
    pub name: String,
    /// The machine it is on: its `session/created` entry's, an entry from before machines had ids being the connected
    /// host's ([`MachineId::read_by`]).
    pub machine: MachineId,
    pub created: Timestamp,
    /// Its PTY process has ended.
    pub exited: bool,
    pub activity: Activity,
    /// It wants the user: it started waiting on them (blocked, or idle) since the user last had it selected.
    pub attention: bool,
    /// Whether it can be resumed, see [`Host::resume_target`].
    pub resume: Result<(), ResumeUnavailable>,
    /// The workspace it was created in, if it was created in one.
    pub workspace: Option<PathBuf>,
    /// It was created in a workspace other than the current one.
    pub elsewhere: bool,
    /// It is waiting for its worktree to be made.
    pub creating_worktree: bool,
    /// Why it failed to start, e.g. its worktree couldn't be made.
    pub failed: Option<String>,
    /// Something went wrong that didn't stop it starting, e.g. a post-create hook failed.
    pub warning: Option<String>,
    /// Its worktree, once made.
    pub worktree: Option<PathBuf>,
    /// Its worktree's branch, see [`HostSession::branch`].
    pub branch: Option<String>,
    /// It is closing, and its worktree is being removed (or kept).
    pub removing_worktree: bool,
    /// Its branch's pull request, if it has one the host could find.
    pub pr: Option<Pr>,
    /// What it runs, see [`crate::host::HostSession::agent`].
    pub agent: Option<AgentKind>,
}

/// A machine in the sidebar: this one, or a trusted peer ([`Client::machines`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineGroup {
    pub id: MachineId,
    /// What it is called: its host's name for itself (this machine), or what this machine's host calls it (a peer).
    pub name: String,
    /// It is this machine.
    pub local: bool,
    pub status: MachineStatus,
}

/// How the client is doing with a machine's host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineStatus {
    Connected,
    /// Being reached.
    Connecting,
    /// Can't be reached now, and why, if known. It is tried again.
    Offline(Option<String>),
    /// Its host speaks another protocol version.
    Incompatible,
}

/// One entry of the sidebar's list, top to bottom ([`Client::sidebar`]).
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarEntry<'a> {
    /// A machine's header, above its sessions. Only while there are peers.
    Machine(MachineGroup),
    Session(&'a ClientSession),
}

/// The client's state and decisions. For each [`ClientEvent`], call [`Client::handle`] and execute the commands
/// in order.
#[derive(Debug, Clone)]
pub struct Client {
    machine: MachineId,
    /// The directory the client started in, sent in its hello.
    cwd: Option<String>,
    /// The build this client runs, sent in its hello and compared with the host's.
    build: Option<BuildId>,
    /// Why the connected host should be restarted, if it should: the restart dialog can be reopened for it.
    restart_offer: Option<HostOutdated>,
    /// A stale host being asked to update itself in place.
    upgrade: Option<Upgrade>,
    /// What the host's last hello answer warned about, until a hello answer without one.
    host_warning: Option<String>,
    connection: Connection,
    /// The host was launched during the current outage. Only a completed hello ends the outage.
    launched: bool,
    next_request: u64,
    /// What this machine's host says.
    local: Feed,
    /// The trusted peers this machine's host reported, and what each one's host says.
    peers: BTreeMap<MachineId, Peer>,
    /// The last seq of each session's log the user has seen it at.
    seen: BTreeMap<SessionId, Seq>,
    sessions: Vec<ClientSession>,
    /// Which host each listed session came from.
    homes: HashMap<SessionId, At>,
    selected: Option<SessionId>,
    /// The user picked the selected session (or created it). Until then the selection follows the top of the list,
    /// so the newest session is selected once the initial replay, which arrives one entry at a time, is done.
    chosen: bool,
    /// A session the user created, to select once it is listed.
    select_when_listed: Option<SessionId>,
    focus: Focus,
    /// The terminal window has focus. Assumed until the terminal says otherwise.
    window_focused: bool,
    popup: Option<Popup>,
    /// The host the open new-session popup or workspace switcher acts on, chosen when it opened and by
    /// [`Intent::PickMachine`]. Only read while one of them is open.
    target: Option<At>,
    /// What removing the worktree of the session in the close confirmation would lose, while that is open.
    close_check: Option<CloseCheck>,
    /// The status request for the close confirmation's session, while unanswered.
    status_request: Option<(RequestId, SessionId)>,
    size: Option<PtySize>,
    /// The index of the first session the sidebar shows, before clamping to the list (see
    /// [`Client::sidebar_scroll`]).
    scroll: usize,
    /// How many sessions the sidebar shows at once, once the TUI has said.
    sidebar_rows: Option<usize>,
    /// The index of the first profile the open picker shows.
    picker_scroll: usize,
    /// The index of the first workspace the open switcher shows.
    switcher_scroll: usize,
    /// The index of the first result the open finder shows.
    finder_scroll: usize,
    /// The orphan worktrees, while the finder is open, and the request asking for them while it is unanswered.
    orphans: Option<Orphans>,
    orphans_request: Option<RequestId>,
    notice: Option<Notice>,
    /// The cleanup view's rows and removals.
    cleanup: Cleanup,
    /// The latest tick's time: what stale sessions are measured against.
    now: Option<jiff::Timestamp>,
    /// What callers outside the UI asked ([`ClientEvent::Ask`]), and what came of it.
    asks: BTreeMap<AskId, Ask>,
    /// How long a peer may take to send its sessions once reached, if it has a limit.
    catch_up_limit: Option<SignedDuration>,
}

/// What one host's hello and log say, and the requests asked of it that it hasn't answered.
#[derive(Debug, Clone, Default)]
struct Feed {
    pending: HashMap<RequestId, Method>,
    /// The session registry, folded from the log entries the host sends.
    registry: Host,
    states: BTreeMap<SessionId, SessionState>,
    /// The last seq folded of each log: the host log (`None`) and each session log.
    seqs: BTreeMap<Option<SessionId>, Seq>,
    /// The host's machine, from its hello answer.
    machine: Option<MachineId>,
    /// What the host's machine is called, from its hello answer. `None` from a host older than names.
    name: Option<String>,
    /// The host's agent profiles, the default first, from its hello answer.
    profiles: Vec<ProfileInfo>,
    default_profile: Option<String>,
    /// Alert when a session newly wants the user, from the host's hello answer.
    bell: bool,
    /// What color the host's machine is drawn in, from its hello answer. `None` when it doesn't say.
    color: Option<MachineColor>,
}

impl Feed {
    /// The subscribe is answered once the catch-up is sent: until then the entries are history, not news.
    fn catching_up(&self) -> bool {
        self.pending.values().any(|m| *m == Method::Subscribe)
    }

    /// Where each log resumes after a reconnect: the last entry folded of each.
    fn subscribe_from(&self) -> SubscribeFrom {
        let mut from = SubscribeFrom::default();
        for (log, seq) in &self.seqs {
            match log {
                None => from.host = Some(*seq),
                Some(session) => {
                    from.sessions.insert(session.clone(), *seq);
                }
            }
        }
        from
    }

    /// Keeps the host's profiles, `default` moved to the front.
    fn set_profiles(&mut self, mut profiles: Vec<ProfileInfo>, default: Option<String>) {
        if let Some(i) = profiles
            .iter()
            .position(|p| Some(&p.name) == default.as_ref())
        {
            let first = profiles.remove(i);
            profiles.insert(0, first);
        }
        self.profiles = profiles;
        self.default_profile = default;
    }
}

/// Which host: this machine's, or a peer's through a tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum At {
    Local,
    Peer(MachineId),
}

/// A trusted peer, and the client's conn to its host.
#[derive(Debug, Clone)]
struct Peer {
    /// What this machine's host calls it.
    name: String,
    feed: Feed,
    link: PeerLink,
    /// Attempts that failed in a row: the wait before the next doubles with each.
    failures: u32,
    /// Why the last attempt failed.
    why: Option<String>,
    /// Its host answered the hello with another protocol version.
    incompatible: bool,
    /// When the conn now Ready opened: its catch-up is due by [`Client::with_catch_up_limit`] after it.
    opened: Option<jiff::Timestamp>,
}

impl Peer {
    fn new(name: String) -> Self {
        Self {
            name,
            feed: Feed::default(),
            link: PeerLink::Connecting,
            failures: 0,
            why: None,
            incompatible: false,
            opened: None,
        }
    }

    fn status(&self) -> MachineStatus {
        match self.link {
            PeerLink::Ready => MachineStatus::Connected,
            PeerLink::Waiting { .. } if self.incompatible => MachineStatus::Incompatible,
            PeerLink::Waiting { .. } => MachineStatus::Offline(self.why.clone()),
            PeerLink::Connecting | PeerLink::Opening { .. } => MachineStatus::Connecting,
        }
    }

    /// How long to wait after the latest failure: [`PEER_RETRY`], doubled for each failure before it, up to
    /// [`PEER_RETRY_MAX`].
    fn backoff(&self) -> SignedDuration {
        let doublings = self.failures.saturating_sub(1).min(16);
        let wait = PEER_RETRY
            .checked_mul(1 << doublings)
            .unwrap_or(PEER_RETRY_MAX);
        wait.min(PEER_RETRY_MAX)
    }
}

/// Where the client is with a peer's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerLink {
    /// A conn to the local host is being opened.
    Connecting,
    /// The conn opened at `since`: the tunnel is asked for, or (once `tunnelled`) the hello. It fails if no answer
    /// comes by [`HELLO_TIMEOUT`] after `since`.
    Opening {
        since: jiff::Timestamp,
        tunnelled: bool,
    },
    Ready,
    /// The last attempt failed: the next is made at `until`, which is set at the first tick after the failure.
    Waiting {
        until: Option<jiff::Timestamp>,
    },
}

/// Asking a stale host to update itself in place.
#[derive(Debug, Clone, Copy)]
struct Upgrade {
    phase: UpgradePhase,
    /// The first tick after it was first asked: [`UPGRADE_DEADLINE`] counts from there.
    since: Option<jiff::Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpgradePhase {
    /// Asked, not answered yet.
    Asked,
    /// It was busy: ask again at `retry_at` (set at the first tick after the answer).
    Busy { retry_at: Option<jiff::Timestamp> },
    /// It is updating: the link drops, and the reconnect's hello tells.
    Accepted,
}

/// A message for the user in the footer. It shows until the next user action, or for [`NOTICE_TIMEOUT`] from the
/// first tick after it appeared.
#[derive(Debug, Clone)]
struct Notice {
    text: String,
    /// The first tick it was shown at.
    since: Option<jiff::Timestamp>,
}

impl Client {
    /// A client on machine `machine`, which it names in its hello.
    pub fn new(machine: MachineId) -> Self {
        Self {
            machine,
            cwd: None,
            build: None,
            restart_offer: None,
            upgrade: None,
            host_warning: None,
            connection: Connection::Idle,
            launched: false,
            next_request: 0,
            local: Feed::default(),
            peers: BTreeMap::new(),
            seen: BTreeMap::new(),
            sessions: Vec::new(),
            homes: HashMap::new(),
            selected: None,
            chosen: false,
            select_when_listed: None,
            focus: Focus::default(),
            window_focused: true,
            popup: None,
            target: None,
            close_check: None,
            status_request: None,
            size: None,
            scroll: 0,
            sidebar_rows: None,
            picker_scroll: 0,
            switcher_scroll: 0,
            finder_scroll: 0,
            orphans: None,
            orphans_request: None,
            notice: None,
            cleanup: Cleanup::default(),
            now: None,
            asks: BTreeMap::new(),
            catch_up_limit: None,
        }
    }

    /// This client, telling the host in its hello that it started in `cwd`.
    pub fn with_cwd(self, cwd: impl Into<String>) -> Self {
        Self {
            cwd: Some(cwd.into()),
            ..self
        }
    }

    /// This client, giving each peer `limit` from when its conn opens to send all its sessions: one that hasn't is
    /// given up on as unreachable (and tried again later), so a script waiting for [`Client::settled`] isn't held up
    /// by it. Without one (the TUI), a peer catches up however long that takes.
    pub fn with_catch_up_limit(self, limit: SignedDuration) -> Self {
        Self {
            catch_up_limit: Some(limit),
            ..self
        }
    }

    /// This client, running `build`: it says so in its hello, and compares the host's with it.
    pub fn with_build(self, build: BuildId) -> Self {
        Self {
            build: Some(build),
            ..self
        }
    }

    /// The workspace of the machine the user is acting on ([`Client::popup_machine`] while the new-session popup or
    /// the switcher is open, else the selected session's), once its host's log says.
    pub fn workspace(&self) -> Option<&Path> {
        self.acting_feed().registry.workspace()
    }

    /// This machine's host's workspace, whichever session is selected: what the finder and cleanup work in.
    pub fn local_workspace(&self) -> Option<&Path> {
        self.local.registry.workspace()
    }

    /// Every workspace the host of the machine the user is acting on has used, the current one first, see
    /// [`Host::workspaces`] and [`Client::workspace`].
    pub fn workspaces(&self) -> &[PathBuf] {
        self.acting_feed().registry.workspaces()
    }

    /// The screen layout this machine's host records (decision 0022), folded from its log: the default until it records
    /// one, and every pane of a session that closed showing nothing. A peer's host's layout is that machine's windows',
    /// not this one's.
    pub fn host_layout(&self) -> Layout {
        self.local.registry.layout()
    }

    /// The layout as this window shows it: [`Client::host_layout`] with every [`Content::Selected`] pane showing this
    /// client's selected session (naming its machine when a peer's host has it), or nothing with none selected.
    pub fn layout(&self) -> Layout {
        let shown = match &self.selected {
            None => Content::Empty,
            Some(id) => match self.homes.get(id) {
                Some(At::Peer(machine)) => Content::peer_session(machine.clone(), id.clone()),
                Some(At::Local) | None => Content::session(id.clone()),
            },
        };
        self.host_layout().resolve_selected(&shown)
    }

    /// Open sessions, grouped by machine (this one first, then the peers by name), each group newest first.
    pub fn sessions(&self) -> &[ClientSession] {
        &self.sessions
    }

    /// The machines, in the sidebar's order: this one, then each trusted peer its host reported, by name.
    pub fn machines(&self) -> Vec<MachineGroup> {
        let id = self
            .local
            .machine
            .clone()
            .unwrap_or_else(|| self.machine.clone());
        let status = match &self.connection {
            Connection::Ready => MachineStatus::Connected,
            Connection::Incompatible => MachineStatus::Incompatible,
            Connection::Failed(why) => MachineStatus::Offline(Some(why.clone())),
            _ => MachineStatus::Connecting,
        };
        let mut machines = vec![MachineGroup {
            name: self.machine_label(&id).to_owned(),
            id,
            local: true,
            status,
        }];
        machines.extend(
            self.peer_order()
                .into_iter()
                .map(|(id, peer)| MachineGroup {
                    id: id.clone(),
                    name: peer.name.clone(),
                    local: false,
                    status: peer.status(),
                }),
        );
        machines
    }

    /// The sidebar's list, top to bottom: each machine's header followed by its sessions, newest first, while there
    /// are peers; just the sessions otherwise.
    pub fn sidebar(&self) -> Vec<SidebarEntry<'_>> {
        if self.peers.is_empty() {
            return self.sessions.iter().map(SidebarEntry::Session).collect();
        }
        let mut entries = Vec::with_capacity(self.sessions.len() + self.peers.len() + 1);
        for (group, at) in self.machines().into_iter().zip(self.host_order()) {
            entries.push(SidebarEntry::Machine(group));
            entries.extend(
                self.sessions
                    .iter()
                    .filter(|s| self.homes.get(&s.id) == Some(&at))
                    .map(SidebarEntry::Session),
            );
        }
        entries
    }

    /// Whether the host of `peer` (this machine's for `None`) is where `session` lives, so what it says of the session
    /// (its terminal output) is to be believed. A session not listed is this machine's host's.
    pub fn is_home(&self, peer: Option<&MachineId>, session: &SessionId) -> bool {
        let at = match peer {
            None => At::Local,
            Some(machine) => At::Peer(machine.clone()),
        };
        self.home(session) == at
    }

    /// Whether listed session `session` is on a peer the client can't reach now: what is shown of it is the last that
    /// was known.
    pub fn offline(&self, session: &SessionId) -> bool {
        match self.homes.get(session) {
            Some(At::Peer(machine)) => self
                .peers
                .get(machine)
                .is_some_and(|p| p.link != PeerLink::Ready),
            _ => false,
        }
    }

    pub fn selected(&self) -> Option<&SessionId> {
        self.selected.as_ref()
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    /// The machine the open new-session popup or workspace switcher acts on. `None` when neither is open.
    pub fn popup_machine(&self) -> Option<&MachineId> {
        match (&self.popup, &self.target) {
            (Some(Popup::PickProfile | Popup::Workspace { .. }), Some(at)) => {
                Some(self.machine_of(at))
            }
            _ => None,
        }
    }

    /// What removing the worktree of the session in the open close confirmation would lose. `None` when no close
    /// confirmation is open, or its session has no worktree.
    pub fn close_check(&self) -> Option<&CloseCheck> {
        match self.popup {
            Some(Popup::ConfirmClose { .. }) => self.close_check.as_ref(),
            _ => None,
        }
    }

    /// The lines of the open context menu, in order: rename and close, then resume on an exited session, saying why
    /// when it can't be resumed. Empty when no menu is open.
    pub fn menu(&self) -> Vec<MenuEntry> {
        let Some(Popup::Menu { session, .. }) = &self.popup else {
            return Vec::new();
        };
        let available = |item| MenuEntry {
            item,
            unavailable: None,
        };
        let mut entries = vec![available(MenuItem::Rename), available(MenuItem::Close)];
        match self.sessions.iter().find(|s| &s.id == session) {
            Some(s) if s.exited => entries.push(MenuEntry {
                item: MenuItem::Resume,
                unavailable: s.resume.err(),
            }),
            _ => {}
        }
        entries
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The index of the first session the sidebar shows.
    pub fn sidebar_scroll(&self) -> usize {
        self.scroll.min(self.max_scroll())
    }

    /// The index of the first profile the open picker shows, before the TUI fits it to the picker's height.
    pub fn picker_scroll(&self) -> usize {
        self.picker_scroll
    }

    /// The index of the first workspace the open switcher shows, before the TUI fits it to the switcher's height.
    pub fn switcher_scroll(&self) -> usize {
        self.switcher_scroll
    }

    /// The index of the first result the open finder shows, before the TUI fits it to the finder's height.
    pub fn finder_scroll(&self) -> usize {
        self.finder_scroll
    }

    /// The open finder's results for its query, best first: see [`finder::results`]. Empty when it isn't open.
    pub fn finder(&self) -> Vec<FinderItem> {
        let Some(Popup::Finder { query }) = &self.popup else {
            return Vec::new();
        };
        finder::results(
            query,
            &self.sessions,
            self.local.machine.as_ref(),
            self.orphans.as_ref(),
        )
    }

    /// The orphan worktrees, as far as the open finder knows. `None` when it isn't open.
    pub fn orphans(&self) -> Option<&Orphans> {
        match self.popup {
            Some(Popup::Finder { .. }) => self.orphans.as_ref(),
            _ => None,
        }
    }

    /// The cleanup view's rows: orphans in the host's order, then stale sessions. They stay while removals run, the view
    /// closed or not, until it is opened again with none running.
    pub fn cleanup_rows(&self) -> &[CleanupRow] {
        &self.cleanup.rows
    }

    /// Where listing the cleanup view's rows is, once it has been opened.
    pub fn cleanup_listing(&self) -> Option<&CleanupListing> {
        self.cleanup.listing.as_ref()
    }

    /// How many rows [`Intent::CleanupRemoveSafe`] would remove.
    pub fn cleanup_safe(&self) -> usize {
        self.cleanup.rows.iter().filter(|r| r.safe()).count()
    }

    /// The index of the first row the cleanup view shows, before clamping to the rows.
    pub fn cleanup_scroll(&self) -> usize {
        self.cleanup.scroll
    }

    /// The machine of the connected host, once it has answered the hello.
    pub fn host_machine(&self) -> Option<&MachineId> {
        self.local.machine.as_ref()
    }

    /// What the connected host's machine is called, once it has answered the hello. `None` from a host older than
    /// machine names.
    pub fn host_name(&self) -> Option<&str> {
        self.local.name.as_deref()
    }

    /// What to call `machine` (a [`ClientSession::machine`]): the connected host's name for its machine (or
    /// [`UNNAMED_HOST`] from a host older than machine names), what that host calls a trusted peer, else the id itself.
    pub fn machine_label<'a>(&'a self, machine: &'a MachineId) -> &'a str {
        match (&self.local.machine, &self.local.name) {
            (Some(host), Some(name)) if host == machine => name,
            (Some(host), None) if host == machine => UNNAMED_HOST,
            _ => match self.peers.get(machine) {
                Some(peer) => &peer.name,
                None => machine.as_str(),
            },
        }
    }

    /// What color `machine`'s name is drawn in: the one its host's hello gives, else [`MachineColor::of`] its id.
    pub fn machine_color(&self, machine: &MachineId) -> MachineColor {
        let peer = self.peers.get(machine).map(|p| &p.feed);
        let from_hello = [Some(&self.local), peer]
            .into_iter()
            .flatten()
            .find(|feed| feed.machine.as_ref() == Some(machine))
            .and_then(|feed| feed.color);
        from_hello.unwrap_or_else(|| MachineColor::of(machine))
    }

    /// The terminal view's size, once the runtime has reported it.
    pub fn size(&self) -> Option<PtySize> {
        self.size
    }

    /// The agent profiles offered for new sessions by the host they start on (the machine chosen in the open
    /// new-session popup, else the selected session's), the default first. Empty until that host has said, or from a
    /// host older than profiles.
    pub fn profiles(&self) -> &[ProfileInfo] {
        &self.acting_feed().profiles
    }

    /// The profile a session runs when none is picked, once the host new sessions start on has said.
    pub fn default_profile(&self) -> Option<&str> {
        self.acting_feed().default_profile.as_deref()
    }

    /// Whether a session on this machine that newly wants the user gives a [`ClientCommand::Alert`] (the host's
    /// `[notify] bell`; each peer's host says for its own sessions).
    pub fn bell(&self) -> bool {
        self.local.bell
    }

    /// The latest thing the host refused, for the user.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|n| n.text.as_str())
    }

    /// How many sessions restarting the host stops: those whose process hasn't exited. `None` until the client knows,
    /// which is once it has caught up with the host's log.
    pub fn sessions_to_stop(&self) -> Option<usize> {
        (self.connection == Connection::Ready && !self.local.catching_up()).then(|| {
            self.sessions
                .iter()
                .filter(|s| !s.exited && self.homes.get(&s.id) == Some(&At::Local))
                .count()
        })
    }

    /// Why the connected host should be restarted, while it should: an older host, one speaking another protocol
    /// version, a stale one, or one that didn't know a request. [`Intent::OfferRestart`] reopens the dialog for it.
    pub fn restart_offer(&self) -> Option<HostOutdated> {
        self.restart_offer
    }

    /// What the host warned about in its last hello answer, e.g. that it runs with the default config: shown until a
    /// hello answer without one, and again on [`Intent::ShowHostWarning`]. A notice times out; this doesn't.
    pub fn host_warning(&self) -> Option<&str> {
        self.host_warning.as_deref()
    }

    /// Why the client gave up, once it has.
    pub fn fatal(&self) -> Option<&str> {
        match &self.connection {
            Connection::Failed(why) => Some(why),
            _ => None,
        }
    }

    /// Why nothing can be asked of this machine's host now or later without the user's help: the client gave up, or
    /// the host speaks another protocol version. `None` while connecting or connected.
    pub fn unusable(&self) -> Option<String> {
        match &self.connection {
            Connection::Failed(why) => Some(why.clone()),
            Connection::Incompatible => Some(INCOMPATIBLE.to_owned()),
            _ => None,
        }
    }

    /// Whether every host the client can reach has sent all its sessions: this machine's host has answered the
    /// subscribe (its catch-up is sent) and the peer list, and each peer listed is caught up likewise or couldn't be
    /// reached (its last attempt failed). What a script lists once it is.
    pub fn settled(&self) -> bool {
        let local_done = self.connection == Connection::Ready
            && !self
                .local
                .pending
                .values()
                .any(|m| matches!(m, Method::Subscribe | Method::PeerList));
        local_done
            && self.peers.values().all(|peer| match peer.link {
                PeerLink::Ready => !peer.feed.catching_up(),
                PeerLink::Waiting { .. } => true,
                PeerLink::Connecting | PeerLink::Opening { .. } => false,
            })
    }

    /// What came of [`ClientEvent::Ask`] `ask`: `None` while it is waiting for its answer, or if it was never asked.
    pub fn answer(&self, ask: AskId) -> Option<&Result<HostResult, AskError>> {
        match self.asks.get(&ask)? {
            Ask::Answered(result) => Some(result),
            Ask::Pending { .. } => None,
        }
    }

    pub fn handle(&mut self, event: ClientEvent) -> Vec<ClientCommand> {
        if matches!(self.connection, Connection::Failed(_)) {
            return Vec::new();
        }
        match event {
            ClientEvent::Start => self.connect(1),
            ClientEvent::Connected { at } => self.connected(&at),
            ClientEvent::ConnectFailed { at, error } => self.connect_failed(&at, &error),
            ClientEvent::Disconnected => self.disconnected(),
            ClientEvent::LaunchFailed { error } => {
                self.fail(format!("could not start the shepherd host: {error}"))
            }
            ClientEvent::TerminateFailed { error } => {
                self.set_notice(format!("could not restart the shepherd host: {error}"));
                Vec::new()
            }
            ClientEvent::Tick { at } => {
                self.now = Some(at.instant());
                self.age_notice(at.instant());
                let mut commands = self.tick_upgrade(at.instant());
                commands.extend(self.tick(at.clone()));
                commands.extend(self.tick_peers(at.instant()));
                commands
            }
            ClientEvent::Host(message) => self.host_message(At::Local, message),
            ClientEvent::User(intent) => self.intent(intent),
            ClientEvent::Peer { machine, event } => self.peer_event(machine, event),
            ClientEvent::Ask {
                ask,
                machine,
                request,
            } => self.ask(ask, machine.as_ref(), request),
        }
    }

    /// A message from the host `at`.
    fn host_message(&mut self, at: At, message: HostMessage) -> Vec<ClientCommand> {
        match message {
            HostMessage::Response(response) => self.response(at, response),
            HostMessage::Notification(HostNotification::LogEntry(entry)) => {
                self.log_entry(&at, &entry.envelope)
            }
            // PTY output feeds the runtime's screens. It changes nothing here.
            HostMessage::Notification(HostNotification::PtyOutput(_)) => Vec::new(),
        }
    }

    // ---------- peers ----------

    /// The peers in the sidebar's order: by name, then id.
    fn peer_order(&self) -> Vec<(&MachineId, &Peer)> {
        let mut peers: Vec<_> = self.peers.iter().collect();
        peers.sort_by(|(a_id, a), (b_id, b)| (&a.name, a_id).cmp(&(&b.name, b_id)));
        peers
    }

    /// Every host, in the sidebar's order: this machine's, then the peers'.
    fn host_order(&self) -> Vec<At> {
        let mut order = vec![At::Local];
        order.extend(
            self.peer_order()
                .into_iter()
                .map(|(id, _)| At::Peer(id.clone())),
        );
        order
    }

    fn feed(&self, at: &At) -> Option<&Feed> {
        match at {
            At::Local => Some(&self.local),
            At::Peer(machine) => self.peers.get(machine).map(|p| &p.feed),
        }
    }

    fn feed_mut(&mut self, at: &At) -> Option<&mut Feed> {
        match at {
            At::Local => Some(&mut self.local),
            At::Peer(machine) => self.peers.get_mut(machine).map(|p| &mut p.feed),
        }
    }

    /// Whether host `at` can be asked anything now.
    fn ready(&self, at: &At) -> bool {
        match at {
            At::Local => self.connection == Connection::Ready,
            At::Peer(machine) => self
                .peers
                .get(machine)
                .is_some_and(|p| p.link == PeerLink::Ready),
        }
    }

    /// The host listed session `session` came from: this machine's for one that isn't listed.
    fn home(&self, session: &SessionId) -> At {
        self.homes.get(session).cloned().unwrap_or(At::Local)
    }

    /// The host the user acts on when nothing says otherwise: the selected session's, or this machine's.
    fn default_host(&self) -> At {
        self.selected
            .as_ref()
            .map_or(At::Local, |session| self.home(session))
    }

    /// The host a popup that chooses its machine opens on: the default one, or this machine's if that can't be
    /// reached now, since an unreachable machine can't be chosen.
    fn popup_default(&self) -> At {
        let at = self.default_host();
        if self.ready(&at) { at } else { At::Local }
    }

    /// The host the user is acting on: the one chosen in the open new-session popup or switcher, else the default.
    fn acting_host(&self) -> At {
        match (&self.popup, &self.target) {
            (Some(Popup::PickProfile | Popup::Workspace { .. }), Some(at)) => at.clone(),
            _ => self.default_host(),
        }
    }

    fn acting_feed(&self) -> &Feed {
        self.feed(&self.acting_host()).unwrap_or(&self.local)
    }

    /// The machine of host `at`.
    fn machine_of<'a>(&'a self, at: &'a At) -> &'a MachineId {
        match at {
            At::Local => self.local.machine.as_ref().unwrap_or(&self.machine),
            At::Peer(machine) => machine,
        }
    }

    /// Peer `machine`, called `name`, is no longer trusted: an open new-session popup or switcher on it goes back to
    /// this machine, saying why, rather than showing this machine's profiles or workspaces as if they were its.
    fn untarget(&mut self, machine: &MachineId, name: &str) {
        if self.target != Some(At::Peer(machine.clone())) {
            return;
        }
        self.target = Some(At::Local);
        self.picker_scroll = 0;
        self.switcher_scroll = 0;
        if matches!(
            self.popup,
            Some(Popup::PickProfile | Popup::Workspace { .. })
        ) {
            self.set_notice(format!("{name} is no longer a trusted machine"));
        }
    }

    /// Chooses `machine` for the open new-session popup or switcher, if one is open and it can be reached now.
    fn pick_machine(&mut self, machine: MachineId) -> Vec<ClientCommand> {
        if !matches!(
            self.popup,
            Some(Popup::PickProfile | Popup::Workspace { .. })
        ) {
            return Vec::new();
        }
        let at = if self.peers.contains_key(&machine) {
            At::Peer(machine)
        } else if *self.machine_of(&At::Local) == machine {
            At::Local
        } else {
            return Vec::new();
        };
        if !self.ready(&at) {
            return Vec::new();
        }
        self.target = Some(at);
        self.picker_scroll = 0;
        self.switcher_scroll = 0;
        Vec::new()
    }

    /// This machine's host listed the peers it trusts: drops those it no longer lists (and their sessions), and dials
    /// new ones.
    fn peers_listed(&mut self, listed: Vec<PeerInfo>) -> Vec<ClientCommand> {
        let mut commands = Vec::new();
        let gone: Vec<MachineId> = self
            .peers
            .keys()
            .filter(|id| !listed.iter().any(|p| &p.id == *id))
            .cloned()
            .collect();
        for machine in gone {
            self.drop_asks(&At::Peer(machine.clone()));
            if let Some(peer) = self.peers.remove(&machine) {
                self.untarget(&machine, &peer.name);
            }
            commands.push(ClientCommand::DisconnectPeer(machine));
        }
        for info in listed {
            match self.peers.get_mut(&info.id) {
                Some(peer) => peer.name = info.name,
                None => {
                    self.peers.insert(info.id.clone(), Peer::new(info.name));
                    commands.push(ClientCommand::ConnectPeer(info.id));
                }
            }
        }
        commands.extend(self.refresh());
        commands
    }

    fn peer_event(&mut self, machine: MachineId, event: PeerEvent) -> Vec<ClientCommand> {
        let Some(peer) = self.peers.get_mut(&machine) else {
            return Vec::new();
        };
        match event {
            PeerEvent::Connected { at } => {
                if peer.link != PeerLink::Connecting {
                    return Vec::new();
                }
                peer.link = PeerLink::Opening {
                    since: at.instant(),
                    tunnelled: false,
                };
                let tunnel = ClientRequest::Tunnel(Tunnel {
                    machine: machine.clone(),
                });
                vec![self.request_to(&At::Peer(machine), tunnel).1]
            }
            PeerEvent::ConnectFailed { at, error } => {
                if peer.link != PeerLink::Connecting {
                    return Vec::new();
                }
                peer.failures += 1;
                peer.why = Some(error);
                let until = at.instant().checked_add(peer.backoff()).ok();
                peer.link = PeerLink::Waiting { until };
                Vec::new()
            }
            PeerEvent::Disconnected => match peer.link {
                // Dialled again at once: it was there a moment ago.
                PeerLink::Ready => {
                    peer.feed.pending.clear();
                    peer.link = PeerLink::Connecting;
                    self.drop_asks(&At::Peer(machine.clone()));
                    let mut commands = vec![ClientCommand::ConnectPeer(machine)];
                    commands.extend(self.refresh());
                    commands
                }
                PeerLink::Opening { .. } => {
                    self.peer_failed(&machine, "the connection closed".to_owned(), false)
                }
                PeerLink::Connecting | PeerLink::Waiting { .. } => Vec::new(),
            },
            PeerEvent::Host(message) => {
                // Its log is only sent once it has said hello; nothing else can be its host's.
                if let HostMessage::Notification(HostNotification::LogEntry(_)) = &message
                    && peer.link != PeerLink::Ready
                {
                    return Vec::new();
                }
                self.host_message(At::Peer(machine), message)
            }
        }
    }

    /// The attempt to reach peer `machine` failed, for `why`: its conn is closed (if `close`), and it is tried again
    /// after its backoff.
    fn peer_failed(&mut self, machine: &MachineId, why: String, close: bool) -> Vec<ClientCommand> {
        let Some(peer) = self.peers.get_mut(machine) else {
            return Vec::new();
        };
        peer.failures += 1;
        peer.why = Some(why);
        peer.feed.pending.clear();
        peer.link = PeerLink::Waiting { until: None };
        self.drop_asks(&At::Peer(machine.clone()));
        let mut commands = Vec::new();
        if close {
            commands.push(ClientCommand::DisconnectPeer(machine.clone()));
        }
        commands.extend(self.refresh());
        commands
    }

    /// Time passes: a peer waiting to be tried again is, once due, and one that doesn't answer is given up on.
    fn tick_peers(&mut self, now: jiff::Timestamp) -> Vec<ClientCommand> {
        let mut commands = Vec::new();
        let limit = self.catch_up_limit;
        let machines: Vec<MachineId> = self.peers.keys().cloned().collect();
        for machine in machines {
            let Some(peer) = self.peers.get_mut(&machine) else {
                continue;
            };
            let overdue = match (limit, peer.opened) {
                (Some(limit), Some(opened)) => now >= opened.checked_add(limit).unwrap_or(opened),
                _ => false,
            };
            match peer.link {
                PeerLink::Ready if overdue && peer.feed.catching_up() => {
                    let why = "it didn't send its sessions in time".to_owned();
                    commands.extend(self.peer_failed(&machine, why, true));
                }
                PeerLink::Waiting { until: None } => {
                    let until = now.checked_add(peer.backoff()).unwrap_or(now);
                    peer.link = PeerLink::Waiting { until: Some(until) };
                }
                PeerLink::Waiting { until: Some(until) } if now >= until => {
                    peer.link = PeerLink::Connecting;
                    commands.push(ClientCommand::ConnectPeer(machine));
                }
                PeerLink::Opening { since, .. }
                    if now >= since.checked_add(HELLO_TIMEOUT).unwrap_or(since) =>
                {
                    let why = "the machine didn't answer".to_owned();
                    commands.extend(self.peer_failed(&machine, why, true));
                }
                _ => {}
            }
        }
        commands
    }

    /// Peer `machine`'s tunnel is open: say hello to its host.
    fn tunnel_open(&mut self, machine: &MachineId) -> Vec<ClientCommand> {
        let Some(peer) = self.peers.get_mut(machine) else {
            return Vec::new();
        };
        let PeerLink::Opening { since, .. } = peer.link else {
            return Vec::new();
        };
        peer.link = PeerLink::Opening {
            since,
            tunnelled: true,
        };
        // This machine's cwd means nothing on that one.
        let hello = Hello {
            version: PROTOCOL_VERSION,
            machine: self.machine.clone(),
            cwd: None,
            build: self.build.clone(),
        };
        let at = At::Peer(machine.clone());
        vec![self.request_to(&at, ClientRequest::Hello(hello)).1]
    }

    /// Peer `machine`'s host answered the hello: subscribe from where its logs were left.
    fn peer_hello(&mut self, machine: &MachineId, ok: HelloOk) -> Vec<ClientCommand> {
        // The tunnel reaches the machine asked for: a host answering as another one is not to be believed.
        if &ok.machine != machine {
            let why = format!("it answered as another machine ({})", ok.machine);
            return self.peer_failed(machine, why, true);
        }
        let Some(peer) = self.peers.get_mut(machine) else {
            return Vec::new();
        };
        if let PeerLink::Opening { since, .. } = peer.link {
            peer.opened = Some(since);
        }
        peer.link = PeerLink::Ready;
        peer.failures = 0;
        peer.why = None;
        peer.incompatible = false;
        peer.feed.machine = Some(ok.machine);
        peer.feed.name = ok.name;
        peer.feed.set_profiles(ok.profiles, ok.default_profile);
        peer.feed.bell = ok.notify.bell;
        peer.feed.color = ok.color;
        let from = peer.feed.subscribe_from();
        let at = At::Peer(machine.clone());
        let subscribe = ClientRequest::Subscribe(Subscribe { from });
        let mut commands = vec![self.request_to(&at, subscribe).1];
        commands.extend(self.refresh());
        if self.selected.as_ref().map(|s| self.home(s)) == Some(at) {
            commands.extend(self.resize_selected());
        }
        commands
    }

    // ---------- connection ----------

    /// Time passes: a waiting retry or an unanswered hello is due.
    fn tick(&mut self, at: Timestamp) -> Vec<ClientCommand> {
        match self.connection {
            Connection::Waiting { attempt, until } if at.instant() >= until => {
                self.connect(attempt + 1)
            }
            Connection::Handshaking { attempt, since }
                if at.instant() >= since.checked_add(HELLO_TIMEOUT).unwrap_or(since) =>
            {
                self.attempt_failed(attempt, at.instant(), "the host didn't answer hello")
            }
            _ => Vec::new(),
        }
    }

    fn connect(&mut self, attempt: u32) -> Vec<ClientCommand> {
        self.connection = Connection::Connecting { attempt };
        vec![ClientCommand::Connect]
    }

    fn connected(&mut self, at: &Timestamp) -> Vec<ClientCommand> {
        let Connection::Connecting { attempt } = self.connection else {
            return Vec::new();
        };
        self.connection = Connection::Handshaking {
            attempt,
            since: at.instant(),
        };
        let hello = Hello {
            version: PROTOCOL_VERSION,
            machine: self.machine.clone(),
            cwd: self.cwd.clone(),
            build: self.build.clone(),
        };
        vec![self.request(ClientRequest::Hello(hello))]
    }

    fn connect_failed(&mut self, at: &Timestamp, error: &str) -> Vec<ClientCommand> {
        let Connection::Connecting { attempt } = self.connection else {
            return Vec::new();
        };
        self.attempt_failed(attempt, at.instant(), error)
    }

    /// Attempt `attempt` failed at `at`: launch the host if this outage hasn't yet, and retry after
    /// [`RETRY_INTERVAL`], or give up after the last attempt.
    fn attempt_failed(
        &mut self,
        attempt: u32,
        at: jiff::Timestamp,
        error: &str,
    ) -> Vec<ClientCommand> {
        self.local.pending.clear();
        if attempt >= MAX_CONNECT_ATTEMPTS {
            return self.fail(format!(
                "could not connect to the shepherd host after {attempt} attempts: {error}. \
                 Run `shepherd host` in a terminal to see why it doesn't start."
            ));
        }
        let mut commands = Vec::new();
        if !self.launched {
            self.launched = true;
            commands.push(ClientCommand::LaunchHost);
        }
        // An instant this close to jiff's maximum can't come from a real clock: retry at once.
        let until = at.checked_add(RETRY_INTERVAL).unwrap_or(at);
        self.connection = Connection::Waiting { attempt, until };
        commands
    }

    fn disconnected(&mut self) -> Vec<ClientCommand> {
        match self.connection {
            // A host that accepts and then drops before answering hello is a failed attempt. Retried from
            // when it connected, so the next tick reconnects.
            Connection::Handshaking { attempt, since } => {
                self.attempt_failed(attempt, since, "the host closed the connection")
            }
            // A new outage.
            Connection::Ready | Connection::Incompatible => {
                self.local.pending.clear();
                self.drop_asks(&At::Local);
                // Only an accepted update outlives its link: the drop is the host re-executing.
                let dropped = self.upgrade.take_if(|u| u.phase != UpgradePhase::Accepted);
                if dropped.is_some() {
                    self.clear_updating();
                }
                // Its answer went with the link.
                if self.orphans_request.take().is_some() {
                    self.orphans = Some(Orphans::Failed(NOT_CONNECTED.to_owned()));
                }
                self.cleanup_disconnected();
                // It was about the host that just went: the next hello decides afresh.
                self.close_restart_dialog();
                self.restart_offer = None;
                self.connect(1)
            }
            _ => Vec::new(),
        }
    }

    /// Asks `machine`'s host (this machine's for `None`) `request` for a caller, or answers at once why it can't.
    fn ask(
        &mut self,
        ask: AskId,
        machine: Option<&MachineId>,
        request: ClientRequest,
    ) -> Vec<ClientCommand> {
        let at = match machine {
            None => At::Local,
            Some(m) if self.local.machine.as_ref() == Some(m) => At::Local,
            Some(m) if self.peers.contains_key(m) => At::Peer(m.clone()),
            Some(m) => {
                let unknown = AskError::UnknownMachine(m.clone());
                self.asks.insert(ask, Ask::Answered(Err(unknown)));
                return Vec::new();
            }
        };
        if !self.ready(&at) {
            let (machine, why) = match &at {
                At::Local => (self.local_label(), self.unusable()),
                At::Peer(m) => (
                    self.machine_label(m).to_owned(),
                    self.peers.get(m).and_then(|p| p.why.clone()),
                ),
            };
            let error = AskError::NotReachable { machine, why };
            self.asks.insert(ask, Ask::Answered(Err(error)));
            return Vec::new();
        }
        let (request, command) = self.request_to(&at, request);
        self.asks.insert(ask, Ask::Pending { at, request });
        vec![command]
    }

    /// What to call this machine's host in a message.
    fn local_label(&self) -> String {
        match &self.local.machine {
            Some(machine) => self.machine_label(machine).to_owned(),
            None => "the shepherd host".to_owned(),
        }
    }

    /// Every ask waiting for host `at` lost its answer with its link.
    fn drop_asks(&mut self, at: &At) {
        let machine = match at {
            At::Local => self.local_label(),
            At::Peer(m) => self.machine_label(m).to_owned(),
        };
        for ask in self.asks.values_mut() {
            if matches!(ask, Ask::Pending { at: asked, .. } if asked == at) {
                *ask = Ask::Answered(Err(AskError::Dropped {
                    machine: machine.clone(),
                }));
            }
        }
    }

    fn fail(&mut self, why: String) -> Vec<ClientCommand> {
        self.connection = Connection::Failed(why);
        vec![ClientCommand::Quit]
    }

    /// A request to this machine's host with a fresh id, remembered so its answer can be typed.
    fn request(&mut self, body: ClientRequest) -> ClientCommand {
        self.request_with_id(body).1
    }

    /// [`Client::request`], with the id it was given.
    fn request_with_id(&mut self, body: ClientRequest) -> (RequestId, ClientCommand) {
        self.request_to(&At::Local, body)
    }

    /// A request to host `at` with a fresh id (unique across hosts), remembered so its answer can be typed.
    fn request_to(&mut self, at: &At, body: ClientRequest) -> (RequestId, ClientCommand) {
        self.next_request += 1;
        let id = RequestId::Number(self.next_request.into());
        if let Some(feed) = self.feed_mut(at) {
            feed.pending.insert(id.clone(), body.method());
        }
        let request = Request {
            id: id.clone(),
            body,
        };
        (id, send_to(at, ClientMessage::Request(request)))
    }

    /// A notification for host `at`, if it can be sent now.
    fn notify_to(&self, at: &At, note: ClientNotification) -> Option<ClientCommand> {
        self.ready(at)
            .then(|| send_to(at, ClientMessage::Notification(note)))
    }

    fn response(&mut self, at: At, response: RawResponse) -> Vec<ClientCommand> {
        let Some(method) = self
            .feed_mut(&at)
            .and_then(|feed| feed.pending.remove(&response.id))
        else {
            return Vec::new();
        };
        let id = response.id.clone();
        let result = match response.typed(method) {
            Ok(typed) => typed.result,
            Err(e) => Err(WireError::Internal(e.to_string())),
        };
        // A caller's: kept for it, and nothing else.
        if let Some(ask) = self.asks.values_mut().find(
            |a| matches!(a, Ask::Pending { at: asked, request } if *asked == at && *request == id),
        ) {
            *ask = Ask::Answered(result.map_err(AskError::Refused));
            return Vec::new();
        }
        if let At::Peer(machine) = &at {
            match (method, result) {
                (Method::Tunnel, Ok(_)) => return self.tunnel_open(machine),
                (Method::Tunnel, Err(e)) => return self.peer_failed(machine, refusal(e), true),
                (Method::Hello, Ok(HostResult::Hello(ok))) => return self.peer_hello(machine, ok),
                (Method::Hello, Err(WireError::VersionMismatch { .. })) => {
                    if let Some(peer) = self.peers.get_mut(machine) {
                        peer.incompatible = true;
                    }
                    let why = "it speaks another protocol version".to_owned();
                    return self.peer_failed(machine, why, true);
                }
                (Method::Hello, Err(e)) => return self.peer_failed(machine, refusal(e), true),
                (method, result) => return self.answered(method, &id, result),
            }
        }
        // A host that doesn't know a request is older than this client: restarting it is offered. Except for the peer
        // list, which only means it has no peers.
        if let Err(WireError::MethodNotFound(_)) = &result
            && method != Method::PeerList
        {
            self.restart_offer.get_or_insert(HostOutdated::Older);
        }
        if self.cleanup_asked(&id) {
            return self.cleanup_response(&id, result);
        }
        match (method, result) {
            (Method::Hello, Ok(HostResult::Hello(ok))) => {
                let update = self.check_build(&ok);
                if let Some(warning) = &ok.warning {
                    self.set_notice(warning.clone());
                }
                self.host_warning = ok.warning;
                self.local.machine = Some(ok.machine.clone());
                self.local.name = ok.name.clone();
                self.local.set_profiles(ok.profiles, ok.default_profile);
                self.local.bell = ok.notify.bell;
                self.local.color = ok.color;
                self.connection = Connection::Ready;
                self.launched = false;
                let subscribe = Subscribe {
                    from: self.local.subscribe_from(),
                };
                let mut commands = vec![self.request(ClientRequest::Subscribe(subscribe))];
                commands.extend(update);
                commands.extend(self.resize_selected());
                commands
            }
            (Method::HostUpgrade, result) => {
                self.upgrade_answered(result);
                Vec::new()
            }
            // Caught up: now the peers can be reached.
            (Method::Subscribe, Ok(_)) => vec![self.request(ClientRequest::PeerList(PeerList {}))],
            (Method::PeerList, Ok(HostResult::PeerList(ok))) => self.peers_listed(ok.peers),
            // A host older than peers has none; a host that can't list them now keeps those already known.
            (Method::PeerList, Err(WireError::MethodNotFound(_))) => self.peers_listed(Vec::new()),
            (Method::PeerList, _) => Vec::new(),
            // The hello ended the outage: the host is there, it just can't be used until it is restarted.
            (Method::Hello, Err(WireError::VersionMismatch { .. })) => {
                self.connection = Connection::Incompatible;
                self.launched = false;
                self.offer_restart(HostOutdated::Older);
                Vec::new()
            }
            (Method::Hello, Err(e)) => self.fail(format!("the shepherd host refused us: {e}")),
            (method, result) => self.answered(method, &id, result),
        }
    }

    /// An answer any host may give, to request `id`.
    fn answered(
        &mut self,
        method: Method,
        id: &RequestId,
        result: Result<HostResult, WireError>,
    ) -> Vec<ClientCommand> {
        match (method, result) {
            (Method::WorkspaceSelect, result) => {
                let open = self.popup.as_mut().and_then(|p| match p {
                    Popup::Workspace { error, .. } => Some(error),
                    _ => None,
                });
                match (result, open) {
                    (Ok(_), Some(_)) => self.popup = None,
                    (Ok(_), None) => {}
                    (Err(e), Some(error)) => *error = Some(refusal(e)),
                    (Err(e), None) => self.set_notice(refusal(e)),
                }
                Vec::new()
            }
            (Method::WorktreeStatus, result) => {
                let asked = self.status_request.take_if(|(asked, _)| asked == id);
                let open = match (&self.popup, asked) {
                    (Some(Popup::ConfirmClose { session }), Some((_, asked))) => *session == asked,
                    _ => false,
                };
                if open {
                    self.close_check = Some(match result {
                        Ok(HostResult::WorktreeStatus(ok)) => match ok.status {
                            None => CloseCheck::Gone,
                            Some(status) if status.losses().is_empty() => CloseCheck::Safe,
                            Some(status) => CloseCheck::AtRisk(status),
                        },
                        Err(e) => CloseCheck::Failed(refusal(e)),
                        Ok(other) => CloseCheck::Failed(format!("unexpected answer {other:?}")),
                    });
                }
                Vec::new()
            }
            (Method::WorktreeList, result) => {
                if self.orphans_request.take_if(|asked| asked == id).is_some() {
                    self.orphans = Some(match result {
                        Ok(HostResult::WorktreeList(ok)) => Orphans::Listed(
                            ok.worktrees
                                .into_iter()
                                .map(|w| Orphan {
                                    path: PathBuf::from(w.path),
                                    branch: w.branch,
                                })
                                .collect(),
                        ),
                        Err(e) => Orphans::Failed(refusal(e)),
                        Ok(other) => Orphans::Failed(format!("unexpected answer {other:?}")),
                    });
                }
                Vec::new()
            }
            (Method::SessionNew, Ok(HostResult::SessionNew(ok))) => {
                if self.listed(&ok.session) {
                    self.chosen = true;
                    self.select(Some(ok.session))
                } else {
                    self.select_when_listed = Some(ok.session);
                    Vec::new()
                }
            }
            (_, Ok(_)) => Vec::new(),
            (_, Err(e)) => {
                self.set_notice(refusal(e));
                Vec::new()
            }
        }
    }

    /// Compares the host's build in its hello answer with this client's (spike T36's table): asks a stale host to
    /// update itself (the request is returned), offers to restart a host older than build identities, and tells a client
    /// that is the odd one out. A client that doesn't know its own build compares nothing.
    fn check_build(&mut self, ok: &HelloOk) -> Option<ClientCommand> {
        self.close_restart_dialog();
        self.restart_offer = None;
        let upgrade = self.upgrade.take();
        let mine = self.build.as_ref()?;
        let installed = ok.installed.as_ref();
        match &ok.build {
            None => self.offer_restart(HostOutdated::Older),
            Some(host) if host.digest == mine.digest => {
                if upgrade.is_some_and(|u| u.phase == UpgradePhase::Accepted) {
                    self.set_notice(UPDATED.to_owned());
                }
            }
            Some(_) if installed == Some(&mine.digest) => return Some(self.ask_upgrade(None)),
            Some(host) if installed == Some(&host.digest) => {
                self.set_notice(ODD_ONE_OUT.to_owned())
            }
            Some(_) => self.set_notice(ALL_DIFFER.to_owned()),
        }
        None
    }

    /// Asks the host to update itself in place, saying so. `since` carries a retry's deadline over.
    fn ask_upgrade(&mut self, since: Option<jiff::Timestamp>) -> ClientCommand {
        self.upgrade = Some(Upgrade {
            phase: UpgradePhase::Asked,
            since,
        });
        self.set_notice(UPDATING.to_owned());
        self.request(ClientRequest::HostUpgrade(HostUpgrade::default()))
    }

    /// The host answered the request to update itself.
    fn upgrade_answered(&mut self, result: Result<HostResult, WireError>) {
        let Some(upgrade) = &mut self.upgrade else {
            return;
        };
        match result {
            Ok(HostResult::Upgrade(UpgradeAnswer::Accepted)) => {
                upgrade.phase = UpgradePhase::Accepted
            }
            Ok(HostResult::Upgrade(UpgradeAnswer::Busy { .. })) => {
                upgrade.phase = UpgradePhase::Busy { retry_at: None }
            }
            Ok(HostResult::Upgrade(UpgradeAnswer::NothingToDo)) => {
                self.upgrade = None;
                self.clear_updating();
            }
            Ok(HostResult::Upgrade(UpgradeAnswer::Unavailable { reason })) => {
                self.cannot_upgrade(Some(format!("the host can't update itself: {reason}")))
            }
            // Older than updating in place: the dialog says what to do.
            Err(WireError::MethodNotFound(_)) => self.cannot_upgrade(None),
            Err(e) => self.cannot_upgrade(Some(refusal(e))),
            Ok(other) => self.cannot_upgrade(Some(format!("unexpected answer {other:?}"))),
        }
    }

    /// The host can't update itself: offers to restart it instead, which asks first.
    fn cannot_upgrade(&mut self, why: Option<String>) {
        self.upgrade = None;
        match why {
            Some(why) => self.set_notice(why),
            None => self.clear_updating(),
        }
        self.offer_restart(HostOutdated::Stale);
    }

    /// Takes down the notice that the host is updating, if it is the one showing.
    fn clear_updating(&mut self) {
        self.notice.take_if(|n| n.text == UPDATING);
    }

    /// Time passes: a busy host is asked again, until the deadline.
    fn tick_upgrade(&mut self, now: jiff::Timestamp) -> Vec<ClientCommand> {
        let Some(upgrade) = &mut self.upgrade else {
            return Vec::new();
        };
        let since = *upgrade.since.get_or_insert(now);
        let UpgradePhase::Busy { retry_at } = &mut upgrade.phase else {
            return Vec::new();
        };
        if now >= since.checked_add(UPGRADE_DEADLINE).unwrap_or(since) {
            let why = "the host stayed busy and didn't update itself".to_owned();
            self.cannot_upgrade(Some(why));
            return Vec::new();
        }
        let due = *retry_at.get_or_insert(now.checked_add(UPGRADE_RETRY).unwrap_or(now));
        if now < due || self.connection != Connection::Ready {
            return Vec::new();
        }
        vec![self.ask_upgrade(Some(since))]
    }

    /// Opens the restart dialog for `why`, and keeps it reopenable ([`Intent::OfferRestart`]) while this host lasts.
    fn offer_restart(&mut self, why: HostOutdated) {
        self.restart_offer = Some(why);
        self.popup = Some(Popup::RestartHost { why });
    }

    /// Closes the restart dialog, if it is open: a click on it must only ever stop the host it was about.
    fn close_restart_dialog(&mut self) {
        self.popup
            .take_if(|p| matches!(p, Popup::RestartHost { .. }));
    }

    // ---------- the session list ----------

    /// A log entry from host `at`. One already folded (its seq is at or before the last of its log from that host) is
    /// dropped.
    fn log_entry(&mut self, at: &At, env: &Envelope) -> Vec<ClientCommand> {
        let Some(feed) = self.feed_mut(at) else {
            return Vec::new();
        };
        let log = env.session().cloned();
        if feed.seqs.get(&log).is_some_and(|&last| env.seq() <= last) {
            return Vec::new();
        }
        feed.seqs.insert(log, env.seq());
        let live = !feed.catching_up();
        feed.registry.apply(env);
        if let Some(id) = env.session() {
            feed.states.entry(id.clone()).or_default().apply(env);
        }
        self.notice_closed(env, live);
        let wanted: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| s.attention)
            .map(|s| s.id.clone())
            .collect();
        let mut commands = self.refresh();
        commands.extend(self.alerts(at, &wanted));
        if *at == At::Local {
            commands.extend(self.cleanup_closed(env));
        }
        commands
    }

    /// The close notice of a `live` `session/closed`: why the host kept the session's worktree, if it says, else its
    /// note.
    fn notice_closed(&mut self, env: &Envelope, live: bool) {
        let (Some(id), Frame::Shepherd(message)) = (env.session(), env.frame()) else {
            return;
        };
        let ShepherdFrame::SessionClosed(closed) = message.body() else {
            return;
        };
        if !live {
            return;
        }
        let name = self
            .sessions
            .iter()
            .find(|s| &s.id == id)
            .map_or(id.as_str(), |s| s.name.as_str());
        match (&closed.worktree_kept, &closed.note) {
            (Some(why), _) => self.set_notice(format!("kept the worktree of {name}: {why}")),
            (None, Some(note)) => self.set_notice(format!("closed {name}: {note}")),
            (None, None) => {}
        }
    }

    /// An alert for each of host `at`'s sessions that wants the user and wasn't in `wanted`, after an entry from it, if
    /// that host's bell is on and it is live. Another host's session can be newly listed by that entry (one of the same
    /// id here closed), but only its own host rings for it.
    fn alerts(&self, at: &At, wanted: &[SessionId]) -> Vec<ClientCommand> {
        let Some(feed) = self.feed(at) else {
            return Vec::new();
        };
        if !feed.bell || feed.catching_up() {
            return Vec::new();
        }
        self.sessions
            .iter()
            .filter(|s| s.attention && !wanted.contains(&s.id) && self.homes.get(&s.id) == Some(at))
            .map(|s| ClientCommand::Alert {
                session: s.id.clone(),
                message: alert_message(s),
            })
            .collect()
    }

    /// Rebuilds the list from the folds, keeps the selection valid, and selects a session the user created.
    fn refresh(&mut self) -> Vec<ClientCommand> {
        let old_index = self.selected_index();
        self.rebuild();
        let homes = &self.homes;
        self.seen.retain(|id, _| homes.contains_key(id));
        // A popup about a session that's gone has nothing left to act on.
        if let Some(
            Popup::Rename { session, .. }
            | Popup::ConfirmClose { session }
            | Popup::Menu { session, .. },
        ) = &self.popup
            && !self.listed(session)
        {
            self.popup = None;
        }
        if let Some(id) = self.select_when_listed.clone()
            && self.listed(&id)
        {
            self.select_when_listed = None;
            self.chosen = true;
            return self.select(Some(id));
        }
        match (&self.selected, old_index) {
            _ if !self.chosen => {
                let first = self.at(0);
                self.select(first)
            }
            (Some(id), _) if self.listed(id) => {
                self.mark_seen();
                Vec::new()
            }
            // The selected session closed: its neighbour below takes its place, or the one above at the bottom.
            (Some(_), Some(i)) => {
                let next = self.at(i.min(self.sessions.len().saturating_sub(1)));
                self.select(next)
            }
            _ => {
                let first = self.at(0);
                self.select(first)
            }
        }
    }

    /// Lists every host's open sessions, host by host in the sidebar's order, each newest first. A session id an
    /// earlier host listed is skipped.
    fn rebuild(&mut self) {
        let starting = SessionState::default();
        let mut sessions = Vec::new();
        let mut homes = HashMap::new();
        for at in self.host_order() {
            let Some(feed) = self.feed(&at) else {
                continue;
            };
            let current = feed.registry.workspace();
            for s in feed.registry.sessions() {
                if homes.contains_key(&s.id) {
                    continue;
                }
                homes.insert(s.id.clone(), at.clone());
                let state = feed.states.get(&s.id).unwrap_or(&starting);
                let last_seen = self.seen.get(&s.id).copied().unwrap_or(Seq::ZERO);
                sessions.push(ClientSession {
                    machine: match &feed.machine {
                        Some(host) => s.machine.read_by(host),
                        None => s.machine,
                    },
                    activity: state.activity().clone(),
                    attention: state.attention(last_seen),
                    id: s.id,
                    name: s.name,
                    created: s.created,
                    exited: s.exited,
                    resume: s.resume,
                    elsewhere: match (&s.workspace, current) {
                        (Some(theirs), Some(current)) => theirs != current,
                        _ => false,
                    },
                    workspace: s.workspace,
                    creating_worktree: s.creating_worktree,
                    failed: s.failed,
                    warning: s.warning,
                    worktree: s.worktree,
                    branch: s.branch,
                    removing_worktree: s.removing_worktree,
                    pr: s.pr,
                    agent: s.agent,
                });
            }
        }
        let hosts = self.host_order();
        for at in hosts {
            if let Some(feed) = self.feed_mut(&at) {
                let open = feed.registry.sessions();
                feed.states.retain(|id, _| open.iter().any(|s| &s.id == id));
            }
        }
        self.sessions = sessions;
        self.homes = homes;
    }

    fn listed(&self, id: &SessionId) -> bool {
        self.sessions.iter().any(|s| &s.id == id)
    }

    fn at(&self, index: usize) -> Option<SessionId> {
        self.sessions.get(index).map(|s| s.id.clone())
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.sessions.iter().position(|s| &s.id == id)
    }

    /// Where the selected session is in the sidebar's list, headers counted.
    fn selected_entry(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.sidebar()
            .iter()
            .position(|e| matches!(e, SidebarEntry::Session(s) if &s.id == id))
    }

    /// How many entries the sidebar lists: the sessions, and a header per machine while there are peers.
    fn sidebar_len(&self) -> usize {
        match self.peers.len() {
            0 => self.sessions.len(),
            peers => self.sessions.len() + peers + 1,
        }
    }

    /// Selects `id`, which is listed (or `None`). If the selection changed, scrolls it into view and resizes it to
    /// the view.
    fn select(&mut self, id: Option<SessionId>) -> Vec<ClientCommand> {
        let changed = self.selected != id;
        self.selected = id;
        self.mark_seen();
        if changed {
            self.scroll_to_selected();
            self.resize_selected().into_iter().collect()
        } else {
            Vec::new()
        }
    }

    /// The selected session has been seen up to its latest entry, if the window has focus: in the background, it's
    /// no more seen than any other.
    fn mark_seen(&mut self) {
        let Some(id) = self.selected.clone().filter(|_| self.window_focused) else {
            return;
        };
        let home = self.home(&id);
        let Some(feed) = self.feed(&home) else {
            return;
        };
        let latest = feed.seqs.get(&Some(id.clone())).copied();
        if let Some(seq) = latest {
            self.seen.insert(id.clone(), seq);
        }
        let last_seen = self.seen.get(&id).copied().unwrap_or(Seq::ZERO);
        let attention = self
            .feed(&home)
            .and_then(|feed| feed.states.get(&id))
            .is_some_and(|s| s.attention(last_seen));
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.attention = attention;
        }
    }

    /// The furthest the sidebar scrolls: the last rows fill it.
    fn max_scroll(&self) -> usize {
        self.sidebar_len()
            .saturating_sub(self.sidebar_rows.unwrap_or(1))
    }

    fn scroll_by(&mut self, rows: i32) {
        self.scroll = scrolled(self.sidebar_scroll(), rows, self.max_scroll());
    }

    /// Scrolls the least that shows the selected session, if the sidebar's height is known.
    fn scroll_to_selected(&mut self) {
        let (Some(index), Some(rows)) = (self.selected_entry(), self.sidebar_rows) else {
            return;
        };
        if rows == 0 {
            return;
        }
        let first = self.sidebar_scroll();
        self.scroll = if index < first {
            index
        } else if index >= first + rows {
            index + 1 - rows
        } else {
            first
        };
    }

    fn step(&mut self, forward: bool) -> Vec<ClientCommand> {
        let len = self.sessions.len();
        if len == 0 {
            return Vec::new();
        }
        let next = match (self.selected_index(), forward) {
            (None, _) => 0,
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
        };
        let id = self.at(next);
        self.chosen = true;
        self.select(id)
    }

    /// Resizes the selected session on its host to the view.
    fn resize_selected(&self) -> Option<ClientCommand> {
        let (session, size) = (self.selected.clone()?, self.size?);
        let home = self.home(&session);
        self.notify_to(
            &home,
            ClientNotification::SessionResize(SessionResize {
                session,
                cols: size.cols,
                rows: size.rows,
            }),
        )
    }

    // ---------- notices ----------

    fn set_notice(&mut self, text: String) {
        self.notice = Some(Notice { text, since: None });
    }

    /// Starts the shown notice's time at its first tick, and clears it once [`NOTICE_TIMEOUT`] has passed.
    fn age_notice(&mut self, now: jiff::Timestamp) {
        let Some(notice) = &mut self.notice else {
            return;
        };
        let since = *notice.since.get_or_insert(now);
        if now >= since.checked_add(NOTICE_TIMEOUT).unwrap_or(since) {
            self.notice = None;
        }
    }

    // ---------- the user ----------

    fn intent(&mut self, intent: Intent) -> Vec<ClientCommand> {
        // Anything the user does clears the notice. The layout reporting sizes isn't the user doing anything, and
        // neither is the window gaining or losing focus: switching back to shepherd shouldn't hide what it said.
        if !matches!(
            intent,
            Intent::Resize(_) | Intent::SidebarRows(_) | Intent::WindowFocus(_)
        ) {
            self.notice = None;
        }
        match intent {
            Intent::NextSession => self.step(true),
            Intent::PrevSession => self.step(false),
            Intent::Input(kind, data) => {
                let Some(session) = self.selected.clone() else {
                    return Vec::new();
                };
                if self.focus != Focus::Terminal || self.popup.is_some() {
                    return Vec::new();
                }
                let home = self.home(&session);
                let note = ClientNotification::SessionInput(SessionInput {
                    session,
                    data,
                    kind,
                });
                self.notify_to(&home, note).into_iter().collect()
            }
            Intent::Resize(size) => {
                self.size = Some(size);
                self.resize_selected().into_iter().collect()
            }
            Intent::SetFocus(focus) => {
                self.focus = focus;
                Vec::new()
            }
            Intent::WindowFocus(focused) => {
                self.window_focused = focused;
                self.mark_seen();
                Vec::new()
            }
            Intent::SelectSession(id) => {
                if !self.listed(&id) {
                    return Vec::new();
                }
                self.chosen = true;
                self.select(Some(id))
            }
            Intent::SelectMachine(machine) => self.select_machine(&machine),
            Intent::ScrollSidebar(rows) => {
                self.scroll_by(rows);
                Vec::new()
            }
            Intent::SidebarRows(rows) => {
                self.sidebar_rows = Some(rows);
                self.scroll_to_selected();
                Vec::new()
            }
            Intent::ScrollPicker { by, shown } => {
                if self.popup == Some(Popup::PickProfile) {
                    let max = self.profiles().len().saturating_sub(shown);
                    self.picker_scroll = scrolled(self.picker_scroll.min(max), by, max);
                }
                Vec::new()
            }
            Intent::ScrollSwitcher { by, shown } => {
                if matches!(self.popup, Some(Popup::Workspace { .. })) {
                    let max = self.workspaces().len().saturating_sub(shown);
                    self.switcher_scroll = scrolled(self.switcher_scroll.min(max), by, max);
                }
                Vec::new()
            }
            Intent::ScrollFinder { by, shown } => {
                if matches!(self.popup, Some(Popup::Finder { .. })) {
                    let max = self.finder().len().saturating_sub(shown);
                    self.finder_scroll = scrolled(self.finder_scroll.min(max), by, max);
                }
                Vec::new()
            }
            Intent::BeginFind => self.begin_find(),
            Intent::EditFind(text) => {
                if let Some(Popup::Finder { query }) = &mut self.popup {
                    *query = text;
                    self.finder_scroll = 0;
                }
                Vec::new()
            }
            Intent::FinderChoose(item) => self.finder_choose(item),
            Intent::NewSession => {
                let host = self.default_host();
                self.new_session(&host, None)
            }
            Intent::BeginNewSession => self.begin_new_session(),
            Intent::PickProfile(name) => {
                if self.popup != Some(Popup::PickProfile) {
                    return Vec::new();
                }
                let host = self.acting_host();
                self.popup = None;
                self.new_session(&host, Some(name))
            }
            Intent::PickMachine(machine) => self.pick_machine(machine),
            Intent::ShowHelp => {
                self.popup = Some(Popup::Help);
                Vec::new()
            }
            Intent::BeginRename => {
                if let Some(session) = self.selected.clone() {
                    self.begin_rename(session);
                }
                Vec::new()
            }
            Intent::EditRename(text) => {
                if let Some(Popup::Rename { name, .. }) = &mut self.popup {
                    *name = text;
                }
                Vec::new()
            }
            Intent::BeginClose => match self.selected.clone() {
                Some(session) => self.begin_close(session),
                None => Vec::new(),
            },
            Intent::BeginCloseSession(session) => self.begin_close(session),
            Intent::OpenMenu { session, at } => {
                if self.listed(&session) {
                    self.popup = Some(Popup::Menu { session, at });
                }
                Vec::new()
            }
            Intent::MenuChoose(item) => {
                let Some(Popup::Menu { session, .. }) =
                    self.popup.take_if(|p| matches!(p, Popup::Menu { .. }))
                else {
                    return Vec::new();
                };
                match item {
                    MenuItem::Rename => {
                        self.begin_rename(session);
                        Vec::new()
                    }
                    MenuItem::Close => self.begin_close(session),
                    MenuItem::Resume => self.resume(session),
                }
            }
            Intent::BeginWorkspace => {
                self.target = Some(self.popup_default());
                self.popup = Some(Popup::Workspace {
                    path: String::new(),
                    error: None,
                });
                self.switcher_scroll = 0;
                Vec::new()
            }
            Intent::EditWorkspacePath(text) => {
                if let Some(Popup::Workspace { path, error }) = &mut self.popup {
                    *path = text;
                    *error = None;
                }
                Vec::new()
            }
            Intent::PickWorkspace(path) => {
                if !matches!(self.popup, Some(Popup::Workspace { .. })) {
                    return Vec::new();
                }
                match path.to_str() {
                    Some(path) => self.select_workspace(path.to_owned()),
                    None => Vec::new(),
                }
            }
            Intent::Confirm => match self.popup.take() {
                Some(Popup::Rename { session, name }) => {
                    let home = self.home(&session);
                    let rename = ClientRequest::SessionRename(SessionRename { session, name });
                    self.user_request_to(&home, rename)
                }
                Some(Popup::ConfirmClose { session }) => {
                    let worktree = match self.close_check.take() {
                        // Removing it would lose work, or the host couldn't say whether it would.
                        Some(CloseCheck::AtRisk(_) | CloseCheck::Failed(_)) => CloseWorktree::Keep,
                        _ => CloseWorktree::IfSafe,
                    };
                    self.close(session, worktree)
                }
                // The switcher stays open until the host answers.
                Some(Popup::Workspace { path, error }) => {
                    let typed = path.trim().to_owned();
                    if typed.is_empty() {
                        let error = Some("type the path of a repo".to_owned());
                        self.popup = Some(Popup::Workspace { path, error });
                        return Vec::new();
                    }
                    self.popup = Some(Popup::Workspace { path, error });
                    self.select_workspace(typed)
                }
                Some(finder @ Popup::Finder { .. }) => {
                    self.popup = Some(finder);
                    match self.finder().into_iter().next() {
                        Some(first) => self.finder_choose(first),
                        None => Vec::new(),
                    }
                }
                Some(Popup::StartInWorktree { worktree }) => self.start_in(worktree),
                // Only a click on its button restarts the host: a key that confirms other dialogs leaves it open.
                Some(restart @ Popup::RestartHost { .. }) => {
                    self.popup = Some(restart);
                    Vec::new()
                }
                // Only a click removes anything.
                Some(Popup::Cleanup) => {
                    self.popup = Some(Popup::Cleanup);
                    Vec::new()
                }
                // While there are peers, its `[Start]` starts the chosen machine's default profile.
                Some(Popup::PickProfile) if !self.peers.is_empty() => {
                    let host = self.target.clone().unwrap_or_else(|| self.default_host());
                    self.new_session(&host, None)
                }
                Some(Popup::Help | Popup::Menu { .. } | Popup::PickProfile) | None => Vec::new(),
            },
            Intent::CloseRemovingWorktree | Intent::CloseKeepingWorktree => {
                // Removing anyway confirms the losses the user was shown, so it needs some shown.
                let worktree = match (&intent, self.close_check()) {
                    (Intent::CloseKeepingWorktree, _) => CloseWorktree::Keep,
                    (_, Some(CloseCheck::AtRisk(shown))) => {
                        CloseWorktree::Remove { confirmed: *shown }
                    }
                    _ => return Vec::new(),
                };
                let Some(Popup::ConfirmClose { session }) = self
                    .popup
                    .take_if(|p| matches!(p, Popup::ConfirmClose { .. }))
                else {
                    return Vec::new();
                };
                self.close(session, worktree)
            }
            Intent::BeginCleanup => self.begin_cleanup(),
            Intent::CleanupRemoveSafe => {
                if self.popup != Some(Popup::Cleanup) {
                    return Vec::new();
                }
                for row in self.cleanup.rows.iter_mut().filter(|r| r.safe()) {
                    row.outcome = Some(CleanupOutcome::Queued);
                }
                self.next_removal()
            }
            Intent::ScrollCleanup { by, shown } => {
                if self.popup == Some(Popup::Cleanup) {
                    let max = self.cleanup.rows.len().saturating_sub(shown);
                    self.cleanup.scroll = scrolled(self.cleanup.scroll, by, max);
                }
                Vec::new()
            }
            Intent::Dismiss => {
                self.popup = None;
                Vec::new()
            }
            Intent::ShowHostWarning => {
                if let Some(warning) = self.host_warning.clone() {
                    self.set_notice(warning);
                }
                Vec::new()
            }
            Intent::OfferRestart => {
                if let Some(why) = self.restart_offer {
                    self.popup = Some(Popup::RestartHost { why });
                }
                Vec::new()
            }
            Intent::RestartHost => {
                if !matches!(self.popup, Some(Popup::RestartHost { .. })) {
                    return Vec::new();
                }
                self.popup = None;
                self.set_notice("restarting the shepherd host…".to_owned());
                vec![ClientCommand::TerminateHost]
            }
            Intent::Quit => vec![ClientCommand::Quit],
        }
    }

    /// Opens the finder with nothing typed, and asks the host for the orphan worktrees.
    fn begin_find(&mut self) -> Vec<ClientCommand> {
        self.popup = Some(Popup::Finder {
            query: String::new(),
        });
        self.finder_scroll = 0;
        if self.connection != Connection::Ready {
            self.orphans = Some(Orphans::Failed(NOT_CONNECTED.to_owned()));
            self.orphans_request = None;
            return Vec::new();
        }
        let (id, ask) = self.request_with_id(ClientRequest::WorktreeList(WorktreeList::default()));
        self.orphans = Some(Orphans::Loading);
        self.orphans_request = Some(id);
        vec![ask]
    }

    /// Chooses `item`, if it is one of the open finder's results: selects a session and gives it the keys, or offers
    /// to start a session in an orphan worktree.
    fn finder_choose(&mut self, item: FinderItem) -> Vec<ClientCommand> {
        if !self.finder().contains(&item) {
            return Vec::new();
        }
        match item {
            FinderItem::Session(id) => {
                self.popup = None;
                self.focus = Focus::Terminal;
                self.chosen = true;
                self.select(Some(id))
            }
            FinderItem::Worktree(worktree) => {
                self.popup = Some(Popup::StartInWorktree { worktree });
                Vec::new()
            }
        }
    }

    /// Asks the host for a session with the default profile in orphan `worktree`, on its branch.
    fn start_in(&mut self, worktree: Orphan) -> Vec<ClientCommand> {
        // Its path came from the wire as text.
        let Some(cwd) = worktree.path.to_str() else {
            return Vec::new();
        };
        self.user_request(ClientRequest::SessionNew(SessionNew {
            profile: None,
            cwd: Some(cwd.to_owned()),
            branch: worktree.branch,
            size: self.view_size(),
            name: None,
        }))
    }

    // ---------- cleanup ----------

    /// Opens the cleanup view. With removals running it shows their rows; otherwise it asks the host for the orphans.
    fn begin_cleanup(&mut self) -> Vec<ClientCommand> {
        self.popup = Some(Popup::Cleanup);
        if self.cleanup.busy() {
            return Vec::new();
        }
        self.cleanup = Cleanup::default();
        if self.connection != Connection::Ready {
            self.cleanup.listing = Some(CleanupListing::Failed(NOT_CONNECTED.to_owned()));
            return Vec::new();
        }
        let (id, ask) =
            self.request_with_id(ClientRequest::WorktreeCleanup(WorktreeCleanup::default()));
        self.cleanup.listing = Some(CleanupListing::Loading);
        self.cleanup.list_request = Some(id);
        vec![ask]
    }

    /// Whether the answer to `id` is meant for the cleanup view: its list, a stale session's status, or a removal.
    fn cleanup_asked(&self, id: &RequestId) -> bool {
        let removal = match &self.cleanup.running {
            Some(Removal::Orphan(asked)) => Some(asked),
            Some(Removal::Session { request, .. }) => request.as_ref(),
            None => None,
        };
        self.cleanup.list_request.as_ref() == Some(id)
            || self.cleanup.checks.contains_key(id)
            || removal == Some(id)
    }

    /// Takes an answer meant for the cleanup view ([`Client::cleanup_asked`]).
    fn cleanup_response(
        &mut self,
        id: &RequestId,
        result: Result<HostResult, WireError>,
    ) -> Vec<ClientCommand> {
        if self.cleanup.list_request.as_ref() == Some(id) {
            self.cleanup.list_request = None;
            return self.cleanup_listed(result);
        }
        if let Some(session) = self.cleanup.checks.remove(id) {
            self.cleanup_checked(&session, result);
            return Vec::new();
        }
        match &mut self.cleanup.running {
            Some(Removal::Orphan(asked)) if asked == id => {
                let outcome = match result {
                    Ok(HostResult::WorktreeRemove(ok)) => match (ok.removed, ok.kept) {
                        (true, _) => CleanupOutcome::Removed,
                        (false, Some(why)) => CleanupOutcome::Kept(why),
                        (false, None) => CleanupOutcome::Gone,
                    },
                    Ok(other) => CleanupOutcome::Kept(format!("unexpected answer {other:?}")),
                    Err(e) => CleanupOutcome::Kept(refusal(e)),
                };
                self.cleanup.finish(outcome);
                self.next_removal()
            }
            Some(Removal::Session { request, .. }) if request.as_ref() == Some(id) => {
                *request = None;
                match result {
                    // Closing: its `session/closed` says how it went.
                    Ok(_) => Vec::new(),
                    Err(e) => {
                        self.cleanup.finish(CleanupOutcome::Kept(refusal(e)));
                        self.next_removal()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    /// The host's list of orphans: their rows, then a status request for each stale session.
    fn cleanup_listed(&mut self, result: Result<HostResult, WireError>) -> Vec<ClientCommand> {
        let ok = match result {
            Ok(HostResult::WorktreeCleanup(ok)) => ok,
            Ok(other) => {
                let why = format!("unexpected answer {other:?}");
                self.cleanup.listing = Some(CleanupListing::Failed(why));
                return Vec::new();
            }
            Err(e) => {
                self.cleanup.listing = Some(CleanupListing::Failed(refusal(e)));
                return Vec::new();
            }
        };
        self.cleanup.listing = Some(CleanupListing::Listed);
        self.cleanup.rows = ok
            .worktrees
            .into_iter()
            .map(|w| CleanupRow {
                item: CleanupItem::Orphan(Orphan {
                    path: PathBuf::from(w.path),
                    branch: w.branch,
                }),
                check: match (w.status, w.error) {
                    (Some(status), _) => CleanupCheck::Checked(status),
                    (None, Some(why)) => CleanupCheck::Failed(why),
                    (None, None) => CleanupCheck::Failed("no status".to_owned()),
                },
                outcome: None,
            })
            .collect();
        let stale: Vec<ClientSession> = self
            .sessions
            .iter()
            .filter(|s| self.stale(s, ok.cleanup_after_days))
            .cloned()
            .collect();
        let mut commands = Vec::new();
        for s in stale {
            let (id, ask) = self.request_with_id(ClientRequest::WorktreeStatus(WorktreeCheck {
                session: s.id.clone(),
            }));
            self.cleanup.checks.insert(id, s.id.clone());
            self.cleanup.rows.push(CleanupRow {
                item: CleanupItem::Session {
                    id: s.id,
                    name: s.name,
                    branch: s.branch,
                },
                check: CleanupCheck::Checking,
                outcome: None,
            });
            commands.push(ask);
        }
        commands
    }

    /// Whether `s` might be stale: it has a worktree in the current workspace and isn't closing, and its PTY exited at
    /// least `days` days before the latest tick. Whether its branch is merged is asked of the host. Only this machine's
    /// host's sessions can be: a peer's session isn't in its registry.
    fn stale(&self, s: &ClientSession, days: u32) -> bool {
        let (Some(now), Some(exited)) = (self.now, self.local.registry.exited_at(&s.id)) else {
            return false;
        };
        let wait = SignedDuration::from_hours(i64::from(days).saturating_mul(24));
        let due = exited
            .instant()
            .checked_add(wait)
            .is_ok_and(|due| due <= now);
        s.exited
            && due
            && !s.removing_worktree
            && !s.creating_worktree
            && s.worktree.is_some()
            && s.workspace.is_some()
            && s.workspace.as_deref() == self.local.registry.workspace()
    }

    /// A stale session's status: its row stays only if its branch is merged.
    fn cleanup_checked(&mut self, session: &SessionId, result: Result<HostResult, WireError>) {
        let check = match result {
            Ok(HostResult::WorktreeStatus(ok)) => match ok.status {
                Some(status) if status.merged_into_default => Some(CleanupCheck::Checked(status)),
                // Not merged, or its worktree is gone: not stale.
                _ => None,
            },
            Ok(other) => Some(CleanupCheck::Failed(format!("unexpected answer {other:?}"))),
            Err(e) => Some(CleanupCheck::Failed(refusal(e))),
        };
        let is_it =
            |r: &CleanupRow| matches!(&r.item, CleanupItem::Session { id, .. } if id == session);
        match check {
            Some(check) => {
                if let Some(row) = self.cleanup.rows.iter_mut().find(|r| is_it(r)) {
                    row.check = check;
                }
            }
            None => self.cleanup.rows.retain(|r| !is_it(r)),
        }
    }

    /// Starts removing the next queued row, if none is being removed. When none is left and the view is closed, a
    /// notice sums up how the removals went.
    fn next_removal(&mut self) -> Vec<ClientCommand> {
        if self.cleanup.running.is_some() {
            return Vec::new();
        }
        let ready = self.connection == Connection::Ready;
        let Some(row) = self
            .cleanup
            .rows
            .iter_mut()
            .find(|r| r.outcome == Some(CleanupOutcome::Queued))
        else {
            if self.popup != Some(Popup::Cleanup) {
                self.notice_cleaned();
            }
            return Vec::new();
        };
        let body = match &row.item {
            _ if !ready => Err(NOT_CONNECTED.to_owned()),
            CleanupItem::Orphan(orphan) => match orphan.path.to_str() {
                Some(path) => Ok(ClientRequest::WorktreeRemove(WorktreeRemove {
                    path: path.to_owned(),
                })),
                None => Err("its path is not UTF-8".to_owned()),
            },
            CleanupItem::Session { id, .. } => Ok(ClientRequest::SessionClose(SessionClose {
                session: id.clone(),
                worktree: CloseWorktree::IfSafe,
            })),
        };
        let body = match body {
            Ok(body) => body,
            Err(why) => {
                row.outcome = Some(CleanupOutcome::Kept(why));
                return self.next_removal();
            }
        };
        row.outcome = Some(CleanupOutcome::Removing);
        let session = match &row.item {
            CleanupItem::Session { id, .. } => Some(id.clone()),
            CleanupItem::Orphan(_) => None,
        };
        let (id, ask) = self.request_with_id(body);
        self.cleanup.running = Some(match session {
            Some(session) => Removal::Session {
                id: session,
                request: Some(id),
            },
            None => Removal::Orphan(id),
        });
        vec![ask]
    }

    /// The `session/closed` of the stale session being removed, if `env` is it: its row's outcome, then the next.
    fn cleanup_closed(&mut self, env: &Envelope) -> Vec<ClientCommand> {
        let (Some(id), Frame::Shepherd(message)) = (env.session(), env.frame()) else {
            return Vec::new();
        };
        let ShepherdFrame::SessionClosed(closed) = message.body() else {
            return Vec::new();
        };
        let waited = matches!(
            &self.cleanup.running,
            Some(Removal::Session { id: waited, .. }) if waited == id
        );
        if !waited {
            return Vec::new();
        }
        let outcome = match (closed.worktree_removed, &closed.worktree_kept) {
            (true, _) => CleanupOutcome::Removed,
            (false, Some(why)) => CleanupOutcome::Kept(why.clone()),
            (false, None) => CleanupOutcome::Gone,
        };
        self.cleanup.finish(outcome);
        self.next_removal()
    }

    /// The link dropped: the removal under way can't be heard of, the queued ones don't start, and the checks and the
    /// list asked for won't be answered.
    fn cleanup_disconnected(&mut self) {
        if self.cleanup.running.is_some() {
            self.cleanup.finish(CleanupOutcome::Unknown);
        }
        for row in &mut self.cleanup.rows {
            if row.outcome == Some(CleanupOutcome::Queued) {
                row.outcome = None;
            }
            if row.check == CleanupCheck::Checking {
                row.check = CleanupCheck::Failed(NOT_CONNECTED.to_owned());
            }
        }
        self.cleanup.checks.clear();
        if self.cleanup.list_request.take().is_some() {
            self.cleanup.listing = Some(CleanupListing::Failed(NOT_CONNECTED.to_owned()));
        }
    }

    /// A notice summing up the removals, for a view closed while they ran.
    fn notice_cleaned(&mut self) {
        let (mut removed, mut kept) = (0, 0);
        for row in &self.cleanup.rows {
            match row.outcome {
                Some(CleanupOutcome::Removed | CleanupOutcome::Gone) => removed += 1,
                Some(CleanupOutcome::Kept(_)) => kept += 1,
                _ => {}
            }
        }
        if removed + kept > 0 {
            self.set_notice(format!("cleanup: removed {removed}, kept {kept}"));
        }
    }

    /// Opens the rename popup for `session`, starting from its current name, if it's listed.
    fn begin_rename(&mut self, session: SessionId) {
        if let Some(s) = self.sessions.iter().find(|s| s.id == session) {
            let name = s.name.clone();
            self.popup = Some(Popup::Rename { session, name });
        }
    }

    /// Asks to close `session`, if it's listed and not closing already. If it has a worktree, asks the host what
    /// removing it would lose.
    fn begin_close(&mut self, session: SessionId) -> Vec<ClientCommand> {
        let Some(listed) = self.sessions.iter().find(|s| s.id == session) else {
            return Vec::new();
        };
        if listed.removing_worktree {
            return Vec::new();
        }
        let has_worktree = listed.worktree.is_some() && !listed.creating_worktree;
        self.popup = Some(Popup::ConfirmClose {
            session: session.clone(),
        });
        self.close_check = None;
        self.status_request = None;
        let home = self.home(&session);
        if !has_worktree || !self.ready(&home) {
            return Vec::new();
        }
        let (id, ask) = self.request_to(
            &home,
            ClientRequest::WorktreeStatus(WorktreeCheck {
                session: session.clone(),
            }),
        );
        self.status_request = Some((id, session));
        self.close_check = Some(CloseCheck::Checking);
        vec![ask]
    }

    /// Asks the host to close `session`, doing `worktree` with its worktree.
    fn close(&mut self, session: SessionId, worktree: CloseWorktree) -> Vec<ClientCommand> {
        self.close_check = None;
        self.status_request = None;
        let home = self.home(&session);
        let close = ClientRequest::SessionClose(SessionClose { session, worktree });
        self.user_request_to(&home, close)
    }

    /// Asks the host to resume exited `session`, then selects it and sizes it to the view: its new PTY starts at the
    /// last size the host knows for it (its initial size if none), and the host handles the request before the resize. A session that can't be resumed gets
    /// a notice saying why instead; a running or unlisted one, nothing.
    fn resume(&mut self, session: SessionId) -> Vec<ClientCommand> {
        let Some(listed) = self.sessions.iter().find(|s| s.id == session) else {
            return Vec::new();
        };
        match listed.resume {
            Ok(()) => {}
            Err(ResumeUnavailable::Running) => return Vec::new(),
            Err(why) => {
                self.set_notice(format!("can't resume: {why}"));
                return Vec::new();
            }
        }
        let home = self.home(&session);
        let resume = ClientRequest::SessionResume(SessionResume {
            session: session.clone(),
        });
        let mut commands = self.user_request_to(&home, resume);
        if commands.is_empty() {
            return commands;
        }
        self.chosen = true;
        self.selected = Some(session);
        self.mark_seen();
        self.scroll_to_selected();
        commands.extend(self.resize_selected());
        commands
    }

    /// The `+ New session` button. While there are peers it opens the popup on the default machine (`popup_default`),
    /// so the machine can be chosen; otherwise it opens the profile picker, or, with at most one profile, starts a
    /// session at once.
    fn begin_new_session(&mut self) -> Vec<ClientCommand> {
        let host = self.popup_default();
        if self.peers.is_empty() {
            match self.profiles() {
                [] => return self.new_session(&host, None),
                [only] => {
                    let name = only.name.clone();
                    return self.new_session(&host, Some(name));
                }
                _ => {}
            }
        }
        self.target = Some(host);
        self.popup = Some(Popup::PickProfile);
        self.picker_scroll = 0;
        Vec::new()
    }

    /// Starts a session running `profile`, or the host's default, on host `at`.
    fn new_session(&mut self, at: &At, profile: Option<String>) -> Vec<ClientCommand> {
        let new = ClientRequest::SessionNew(SessionNew {
            profile,
            size: self.view_size(),
            ..SessionNew::default()
        });
        self.user_request_to(at, new)
    }

    /// A click on `machine`'s header: a peer waiting to be tried again is tried now; otherwise its newest session is
    /// selected.
    fn select_machine(&mut self, machine: &MachineId) -> Vec<ClientCommand> {
        if let Some(peer) = self.peers.get_mut(machine)
            && matches!(peer.link, PeerLink::Waiting { .. })
        {
            peer.link = PeerLink::Connecting;
            return vec![ClientCommand::ConnectPeer(machine.clone())];
        }
        let at = match self.peers.contains_key(machine) {
            true => At::Peer(machine.clone()),
            false => At::Local,
        };
        let newest = self
            .sessions
            .iter()
            .find(|s| self.homes.get(&s.id) == Some(&at))
            .map(|s| s.id.clone());
        match newest {
            Some(id) => {
                self.chosen = true;
                self.select(Some(id))
            }
            None => Vec::new(),
        }
    }

    /// The terminal view's size, for a new session's PTY to spawn at.
    fn view_size(&self) -> Option<ViewSize> {
        self.size.map(|s| ViewSize {
            cols: s.cols,
            rows: s.rows,
        })
    }

    /// Asks the switcher's machine's host to select workspace `path`. The switcher stays open until it answers.
    fn select_workspace(&mut self, path: String) -> Vec<ClientCommand> {
        let host = self.acting_host();
        self.user_request_to(
            &host,
            ClientRequest::WorkspaceSelect(WorkspaceSelect { path }),
        )
    }

    /// A request the user asked for of this machine's host. Dropped, with a notice, while not connected.
    fn user_request(&mut self, body: ClientRequest) -> Vec<ClientCommand> {
        self.user_request_to(&At::Local, body)
    }

    /// A request the user asked for of host `at`. Dropped, with a notice, while not connected to it.
    fn user_request_to(&mut self, at: &At, body: ClientRequest) -> Vec<ClientCommand> {
        if !self.ready(at) {
            let why = match at {
                At::Local => NOT_CONNECTED.to_owned(),
                At::Peer(machine) => format!("not connected to {}", self.machine_label(machine)),
            };
            self.set_notice(why);
            return Vec::new();
        }
        vec![self.request_to(at, body).1]
    }
}

/// A command sending `message` to host `at`.
fn send_to(at: &At, message: ClientMessage) -> ClientCommand {
    match at {
        At::Local => ClientCommand::Send(message),
        At::Peer(machine) => ClientCommand::SendPeer {
            machine: machine.clone(),
            message,
        },
    }
}

/// What a request the host refused says, for the user. A method the host doesn't know means it is older than this
/// client. Otherwise the host's reason for invalid params is the whole story, without the error's kind.
fn refusal(e: WireError) -> String {
    match e {
        WireError::MethodNotFound(_) => OLDER_HOST.to_owned(),
        WireError::InvalidParams(why) => why,
        e => e.to_string(),
    }
}

/// Why something can't be asked of the host now.
const NOT_CONNECTED: &str = "not connected to the shepherd host";
/// What the user is told when the host doesn't know a request's method.
const UPDATING: &str = "host is older — updating…";
const UPDATED: &str = "host updated";
const INCOMPATIBLE: &str =
    "the shepherd host speaks another protocol version than this client: restart it to use this";
const OLDER_HOST: &str = "the shepherd host is older than this client: restart it to use this";
/// The notice for a client whose build is neither the host's nor the one installed at the host's path, while the host
/// runs the installed one.
const ODD_ONE_OUT: &str =
    "this window runs another build than the host: restart it to use the installed one";
/// The notice when this client, the host and the binary installed at the host's path are three different builds.
const ALL_DIFFER: &str =
    "this window and the host run different builds, and neither is the installed one";

/// What an alert about `session` says. It starts with fixed text, so a session's name can never lead it: OSC 9 text
/// beginning `<digit>;` is a subcommand (progress, cwd) in ConEmu-style terminals.
fn alert_message(session: &ClientSession) -> String {
    let name = &session.name;
    match &session.activity {
        Activity::NeedsPermission { title } => {
            format!("shepherd: {name} needs permission: {title}")
        }
        Activity::NeedsInput => format!("shepherd: {name} needs input"),
        _ => format!("shepherd: {name} is idle"),
    }
}

/// A list's first shown row `from`, moved `rows` (down for positive) and kept within `0..=max`.
fn scrolled(from: usize, rows: i32, max: usize) -> usize {
    let by = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
    let to = if rows < 0 {
        from.saturating_sub(by)
    } else {
        from.saturating_add(by)
    };
    to.min(max)
}
