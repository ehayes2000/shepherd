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
//! Workspaces: the host's workspace and the ones used before are folded from its log like the sessions. Every session
//! is listed, whatever its workspace; one created in a workspace other than the current one is marked
//! ([`ClientSession::elsewhere`]) rather than hidden, so nothing running is ever out of sight. The switcher
//! ([`Popup::Workspace`]) asks the host to select a workspace and stays open until the host answers: a refusal is shown
//! in it.
//!
//! Closing: the close confirmation of a session with a worktree asks the host what removing the worktree would lose
//! ([`Client::close_check`]). If it loses nothing, confirming lets the host remove it (if that is still true when it
//! closes); if it would lose work, the confirmation lists what and offers to remove it anyway or keep it, and confirming
//! keeps it. A session is listed while its worktree is being removed, and can't be closed again. When the host keeps a
//! worktree it tried to remove, the reason is a notice (live entries only, not the catch-up).
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
//! [`Client::handle`] is pure. Request ids are a per-client counter, and time arrives in events
//! ([`ClientEvent::ConnectFailed`], [`ClientEvent::Tick`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use jiff::SignedDuration;

use crate::acpp::{Envelope, Frame, RequestId, Seq, ShepherdFrame, Timestamp};
use crate::host::{Host, HostSession, ResumeUnavailable};
use crate::ids::{MachineId, SessionId};
use crate::ports::pty::PtySize;
use crate::ports::worktrees::WorktreeStatus;
use crate::session::{Activity, SessionState};
use crate::wire::{
    BuildId, ClientMessage, ClientNotification, ClientRequest, CloseWorktree, Hello, HelloOk,
    HostMessage, HostNotification, HostResult, HostUpgrade, InputKind, Method, PROTOCOL_VERSION,
    ProfileInfo, RawResponse, Request, SessionClose, SessionInput, SessionNew, SessionRename,
    SessionResize, SessionResume, Subscribe, SubscribeFrom, UpgradeAnswer, WireError,
    WorkspaceSelect, WorktreeCheck,
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
    /// Start a session with the default profile.
    NewSession,
    /// The `+ New session` button: open the profile picker, or, with at most one profile, start a session at once.
    BeginNewSession,
    /// Start a session with this profile, from the open profile picker. Ignored when the picker isn't open.
    PickProfile(String),
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
    /// Accept the open popup: send the rename, the close, or the workspace switcher's typed path. A close whose worktree
    /// would lose work keeps the worktree.
    Confirm,
    /// Close the session of the open close confirmation, removing its worktree even though that loses what the
    /// confirmation showed. Ignored unless it showed something removing would lose.
    CloseRemovingWorktree,
    /// Close the session of the open close confirmation, keeping its worktree.
    CloseKeepingWorktree,
    /// Close the open popup or menu without doing anything.
    Dismiss,
    /// Reopen the restart dialog, when the host should be restarted ([`Client::restart_offer`]); ignored otherwise.
    OfferRestart,
    /// Restart the host, from the open restart dialog (a click on its `[Restart host]`). Ignored when that isn't open.
    RestartHost,
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
    /// It is closing, and its worktree is being removed (or kept).
    pub removing_worktree: bool,
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
    connection: Connection,
    /// The host was launched during the current outage. Only a completed hello ends the outage.
    launched: bool,
    next_request: u64,
    pending: HashMap<RequestId, Method>,
    /// The session registry, folded from the log entries the host sends.
    registry: Host,
    states: BTreeMap<SessionId, SessionState>,
    /// The last seq folded of each log: the host log (`None`) and each session log.
    seqs: BTreeMap<Option<SessionId>, Seq>,
    /// The last seq of each session's log the user has seen it at.
    seen: BTreeMap<SessionId, Seq>,
    sessions: Vec<ClientSession>,
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
    notice: Option<Notice>,
    /// The host's agent profiles, the default first, from its hello answer.
    profiles: Vec<ProfileInfo>,
    default_profile: Option<String>,
    /// Alert when a session newly wants the user, from the host's hello answer.
    bell: bool,
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
            connection: Connection::Idle,
            launched: false,
            next_request: 0,
            pending: HashMap::new(),
            registry: Host::new(),
            states: BTreeMap::new(),
            seqs: BTreeMap::new(),
            seen: BTreeMap::new(),
            sessions: Vec::new(),
            selected: None,
            chosen: false,
            select_when_listed: None,
            focus: Focus::default(),
            window_focused: true,
            popup: None,
            close_check: None,
            status_request: None,
            size: None,
            scroll: 0,
            sidebar_rows: None,
            picker_scroll: 0,
            switcher_scroll: 0,
            notice: None,
            profiles: Vec::new(),
            default_profile: None,
            bell: false,
        }
    }

    /// This client, telling the host in its hello that it started in `cwd`.
    pub fn with_cwd(self, cwd: impl Into<String>) -> Self {
        Self {
            cwd: Some(cwd.into()),
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

    /// The host's workspace, once its log says.
    pub fn workspace(&self) -> Option<&Path> {
        self.registry.workspace()
    }

    /// Every workspace the host has used, the current one first, see [`Host::workspaces`].
    pub fn workspaces(&self) -> &[PathBuf] {
        self.registry.workspaces()
    }

    /// Open sessions, newest first.
    pub fn sessions(&self) -> &[ClientSession] {
        &self.sessions
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

    /// The terminal view's size, once the runtime has reported it.
    pub fn size(&self) -> Option<PtySize> {
        self.size
    }

    /// The agent profiles the host offers for new sessions, the default first. Empty until the host has said, or from
    /// a host older than profiles.
    pub fn profiles(&self) -> &[ProfileInfo] {
        &self.profiles
    }

    /// The profile a session runs when none is picked, once the host has said.
    pub fn default_profile(&self) -> Option<&str> {
        self.default_profile.as_deref()
    }

    /// Whether a session that newly wants the user gives a [`ClientCommand::Alert`] (the host's `[notify] bell`).
    pub fn bell(&self) -> bool {
        self.bell
    }

    /// The latest thing the host refused, for the user.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|n| n.text.as_str())
    }

    /// How many sessions restarting the host stops: those whose process hasn't exited. `None` until the client knows,
    /// which is once it has caught up with the host's log.
    pub fn sessions_to_stop(&self) -> Option<usize> {
        (self.connection == Connection::Ready && !self.catching_up())
            .then(|| self.sessions.iter().filter(|s| !s.exited).count())
    }

    /// Why the connected host should be restarted, while it should: an older host, one speaking another protocol
    /// version, a stale one, or one that didn't know a request. [`Intent::OfferRestart`] reopens the dialog for it.
    pub fn restart_offer(&self) -> Option<HostOutdated> {
        self.restart_offer
    }

    /// Why the client gave up, once it has.
    pub fn fatal(&self) -> Option<&str> {
        match &self.connection {
            Connection::Failed(why) => Some(why),
            _ => None,
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
                self.age_notice(at.instant());
                let mut commands = self.tick_upgrade(at.instant());
                commands.extend(self.tick(at));
                commands
            }
            ClientEvent::Host(HostMessage::Response(response)) => self.response(response),
            ClientEvent::Host(HostMessage::Notification(HostNotification::LogEntry(entry))) => {
                self.log_entry(&entry.envelope)
            }
            // PTY output feeds the runtime's screens. It changes nothing here.
            ClientEvent::Host(HostMessage::Notification(HostNotification::PtyOutput(_))) => {
                Vec::new()
            }
            ClientEvent::User(intent) => self.intent(intent),
        }
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
        self.pending.clear();
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
                self.pending.clear();
                // Only an accepted update outlives its link: the drop is the host re-executing.
                let dropped = self.upgrade.take_if(|u| u.phase != UpgradePhase::Accepted);
                if dropped.is_some() {
                    self.clear_updating();
                }
                // It was about the host that just went: the next hello decides afresh.
                self.close_restart_dialog();
                self.restart_offer = None;
                self.connect(1)
            }
            _ => Vec::new(),
        }
    }

    fn fail(&mut self, why: String) -> Vec<ClientCommand> {
        self.connection = Connection::Failed(why);
        vec![ClientCommand::Quit]
    }

    /// A request with a fresh id, remembered so its answer can be typed.
    fn request(&mut self, body: ClientRequest) -> ClientCommand {
        self.request_with_id(body).1
    }

    /// [`Client::request`], with the id it was given.
    fn request_with_id(&mut self, body: ClientRequest) -> (RequestId, ClientCommand) {
        self.next_request += 1;
        let id = RequestId::Number(self.next_request.into());
        self.pending.insert(id.clone(), body.method());
        let request = Request {
            id: id.clone(),
            body,
        };
        (id, ClientCommand::Send(ClientMessage::Request(request)))
    }

    fn notify(&self, note: ClientNotification) -> Option<ClientCommand> {
        (self.connection == Connection::Ready)
            .then_some(ClientCommand::Send(ClientMessage::Notification(note)))
    }

    fn response(&mut self, response: RawResponse) -> Vec<ClientCommand> {
        let Some(method) = self.pending.remove(&response.id) else {
            return Vec::new();
        };
        let id = response.id.clone();
        let result = match response.typed(method) {
            Ok(typed) => typed.result,
            Err(e) => Err(WireError::Internal(e.to_string())),
        };
        // A host that doesn't know a request is older than this client: restarting it is offered.
        if let Err(WireError::MethodNotFound(_)) = &result {
            self.restart_offer.get_or_insert(HostOutdated::Older);
        }
        match (method, result) {
            (Method::Hello, Ok(HostResult::Hello(ok))) => {
                let update = self.check_build(&ok);
                if let Some(warning) = ok.warning {
                    self.set_notice(warning);
                }
                self.set_profiles(ok.profiles, ok.default_profile);
                self.bell = ok.notify.bell;
                self.connection = Connection::Ready;
                self.launched = false;
                let subscribe = Subscribe {
                    from: self.subscribe_from(),
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
            // The hello ended the outage: the host is there, it just can't be used until it is restarted.
            (Method::Hello, Err(WireError::VersionMismatch { .. })) => {
                self.connection = Connection::Incompatible;
                self.launched = false;
                self.offer_restart(HostOutdated::Older);
                Vec::new()
            }
            (Method::Hello, Err(e)) => self.fail(format!("the shepherd host refused us: {e}")),
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
                let asked = self.status_request.take_if(|(asked, _)| *asked == id);
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

    // ---------- the session list ----------

    fn log_entry(&mut self, env: &Envelope) -> Vec<ClientCommand> {
        let log = env.session().cloned();
        if self.seqs.get(&log).is_some_and(|&last| env.seq() <= last) {
            return Vec::new();
        }
        self.seqs.insert(log, env.seq());
        self.notice_kept_worktree(env);
        self.registry.apply(env);
        if let Some(id) = env.session() {
            self.states.entry(id.clone()).or_default().apply(env);
        }
        let wanted: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| s.attention)
            .map(|s| s.id.clone())
            .collect();
        let mut commands = self.refresh();
        commands.extend(self.alerts(&wanted));
        commands
    }

    /// A notice saying why the host kept the worktree of a session it closed, for a live `session/closed` that says.
    fn notice_kept_worktree(&mut self, env: &Envelope) {
        let (Some(id), Frame::Shepherd(message)) = (env.session(), env.frame()) else {
            return;
        };
        let ShepherdFrame::SessionClosed(closed) = message.body() else {
            return;
        };
        let (Some(why), false) = (&closed.worktree_kept, self.catching_up()) else {
            return;
        };
        let name = self
            .sessions
            .iter()
            .find(|s| &s.id == id)
            .map_or(id.as_str(), |s| s.name.as_str());
        self.set_notice(format!("kept the worktree of {name}: {why}"));
    }

    /// The subscribe is answered once the catch-up is sent: until then the entries are history, not news.
    fn catching_up(&self) -> bool {
        self.pending.values().any(|m| *m == Method::Subscribe)
    }

    /// An alert for each session that wants the user and wasn't in `wanted`, if the bell is on and the client is live.
    fn alerts(&self, wanted: &[SessionId]) -> Vec<ClientCommand> {
        if !self.bell || self.catching_up() {
            return Vec::new();
        }
        self.sessions
            .iter()
            .filter(|s| s.attention && !wanted.contains(&s.id))
            .map(|s| ClientCommand::Alert {
                session: s.id.clone(),
                message: alert_message(s),
            })
            .collect()
    }

    /// Rebuilds the list from the folds, keeps the selection valid, and selects a session the user created.
    fn refresh(&mut self) -> Vec<ClientCommand> {
        let old_index = self.selected_index();
        let open = self.registry.sessions();
        self.states.retain(|id, _| open.iter().any(|s| &s.id == id));
        self.seen.retain(|id, _| self.states.contains_key(id));
        self.rebuild(open);
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

    fn rebuild(&mut self, open: Vec<HostSession>) {
        let starting = SessionState::default();
        let current = self.registry.workspace();
        self.sessions = open
            .into_iter()
            .map(|s| {
                let state = self.states.get(&s.id).unwrap_or(&starting);
                let last_seen = self.seen.get(&s.id).copied().unwrap_or(Seq::ZERO);
                ClientSession {
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
                    removing_worktree: s.removing_worktree,
                }
            })
            .collect();
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
        if let Some(&seq) = self.seqs.get(&Some(id.clone())) {
            self.seen.insert(id.clone(), seq);
        }
        let last_seen = self.seen.get(&id).copied().unwrap_or(Seq::ZERO);
        let attention = self.states.get(&id).is_some_and(|s| s.attention(last_seen));
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.attention = attention;
        }
    }

    /// The furthest the sidebar scrolls: the last rows fill it.
    fn max_scroll(&self) -> usize {
        self.sessions
            .len()
            .saturating_sub(self.sidebar_rows.unwrap_or(1))
    }

    fn scroll_by(&mut self, rows: i32) {
        self.scroll = scrolled(self.sidebar_scroll(), rows, self.max_scroll());
    }

    /// Scrolls the least that shows the selected session, if the sidebar's height is known.
    fn scroll_to_selected(&mut self) {
        let (Some(index), Some(rows)) = (self.selected_index(), self.sidebar_rows) else {
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

    fn resize_selected(&self) -> Option<ClientCommand> {
        let (session, size) = (self.selected.clone()?, self.size?);
        self.notify(ClientNotification::SessionResize(SessionResize {
            session,
            cols: size.cols,
            rows: size.rows,
        }))
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
                let note = ClientNotification::SessionInput(SessionInput {
                    session,
                    data,
                    kind,
                });
                self.notify(note).into_iter().collect()
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
                    let max = self.profiles.len().saturating_sub(shown);
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
            Intent::NewSession => self.new_session(None),
            Intent::BeginNewSession => match &self.profiles[..] {
                [] => self.new_session(None),
                [only] => {
                    let name = only.name.clone();
                    self.new_session(Some(name))
                }
                _ => {
                    self.popup = Some(Popup::PickProfile);
                    self.picker_scroll = 0;
                    Vec::new()
                }
            },
            Intent::PickProfile(name) => {
                if self.popup != Some(Popup::PickProfile) {
                    return Vec::new();
                }
                self.popup = None;
                self.new_session(Some(name))
            }
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
                    self.user_request(ClientRequest::SessionRename(SessionRename {
                        session,
                        name,
                    }))
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
                // Only a click on its button restarts the host: a key that confirms other dialogs leaves it open.
                Some(restart @ Popup::RestartHost { .. }) => {
                    self.popup = Some(restart);
                    Vec::new()
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
            Intent::Dismiss => {
                self.popup = None;
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
        if !has_worktree || self.connection != Connection::Ready {
            return Vec::new();
        }
        let (id, ask) = self.request_with_id(ClientRequest::WorktreeStatus(WorktreeCheck {
            session: session.clone(),
        }));
        self.status_request = Some((id, session));
        self.close_check = Some(CloseCheck::Checking);
        vec![ask]
    }

    /// Asks the host to close `session`, doing `worktree` with its worktree.
    fn close(&mut self, session: SessionId, worktree: CloseWorktree) -> Vec<ClientCommand> {
        self.close_check = None;
        self.status_request = None;
        self.user_request(ClientRequest::SessionClose(SessionClose {
            session,
            worktree,
        }))
    }

    /// Asks the host to resume exited `session`, then selects it and sizes it to the view: its new PTY starts at the
    /// host's initial size, and the host handles the request before the resize. A session that can't be resumed gets
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
        let mut commands = self.user_request(ClientRequest::SessionResume(SessionResume {
            session: session.clone(),
        }));
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

    /// Starts a session running `profile`, or the host's default.
    fn new_session(&mut self, profile: Option<String>) -> Vec<ClientCommand> {
        self.user_request(ClientRequest::SessionNew(SessionNew { profile, cwd: None }))
    }

    /// Asks the host to select workspace `path`. The switcher stays open until it answers.
    fn select_workspace(&mut self, path: String) -> Vec<ClientCommand> {
        self.user_request(ClientRequest::WorkspaceSelect(WorkspaceSelect { path }))
    }

    /// A request the user asked for. Dropped, with a notice, while not connected.
    fn user_request(&mut self, body: ClientRequest) -> Vec<ClientCommand> {
        if self.connection != Connection::Ready {
            self.set_notice("not connected to the shepherd host".to_owned());
            return Vec::new();
        }
        vec![self.request(body)]
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

/// What the user is told when the host doesn't know a request's method.
const UPDATING: &str = "host is older — updating…";
const UPDATED: &str = "host updated";
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
