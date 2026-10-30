//! The client's view state and decisions: connecting to a host, the merged session list, selection, focus and
//! popups, and turning what the user means into wire messages.
//!
//! [`Client::handle`] is pure. Request ids are a per-client counter, and time arrives in events
//! ([`ClientEvent::ConnectFailed`], [`ClientEvent::Tick`]).

use std::collections::{BTreeMap, HashMap};

use jiff::SignedDuration;

use crate::acpp::{Envelope, RequestId, Seq, Timestamp};
use crate::host::{Host, HostSession};
use crate::ids::{MachineId, SessionId};
use crate::ports::pty::PtySize;
use crate::session::{Activity, SessionState};
use crate::wire::{
    ClientMessage, ClientNotification, ClientRequest, Hello, HostMessage, HostNotification,
    HostResult, Method, PROTOCOL_VERSION, ProfileInfo, RawResponse, Request, SessionClose,
    SessionInput, SessionNew, SessionRename, SessionResize, Subscribe, SubscribeFrom, WireError,
};

/// Connect attempts per outage, the first included, before the client gives up.
pub const MAX_CONNECT_ATTEMPTS: u32 = 50;
/// How long after a failed connect the next attempt is made.
pub const RETRY_INTERVAL: SignedDuration = SignedDuration::from_millis(100);
/// How long a connected host has to answer the hello before the attempt counts as failed.
pub const HELLO_TIMEOUT: SignedDuration = SignedDuration::from_secs(5);

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
    /// Bytes typed into the terminal, already encoded for it.
    Input(Vec<u8>),
    /// The terminal view's size changed.
    Resize(PtySize),
    SetFocus(Focus),
    /// Select this session (a click on its row). A session that isn't listed is ignored.
    SelectSession(SessionId),
    /// Scroll the session list by this many rows: down for positive, up for negative. The selection stays put, even
    /// out of view.
    ScrollSidebar(i32),
    /// How many sessions the sidebar shows at once, from the TUI's layout. The list scrolls to fit.
    SidebarRows(usize),
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
    /// Accept the open popup: send the rename or the close.
    Confirm,
    /// Close the open popup or menu without doing anything.
    Dismiss,
    /// Detach. Sessions keep running on the host.
    Quit,
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
    /// It wants the user: blocked on them, or idle since the user last had it selected.
    pub attention: bool,
}

/// The client's state and decisions. For each [`ClientEvent`], call [`Client::handle`] and execute the commands
/// in order.
#[derive(Debug, Clone)]
pub struct Client {
    machine: MachineId,
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
    popup: Option<Popup>,
    size: Option<PtySize>,
    /// The index of the first session the sidebar shows, before clamping to the list (see
    /// [`Client::sidebar_scroll`]).
    scroll: usize,
    /// How many sessions the sidebar shows at once, once the TUI has said.
    sidebar_rows: Option<usize>,
    notice: Option<String>,
    /// The host's agent profiles, the default first, from its hello answer.
    profiles: Vec<ProfileInfo>,
    default_profile: Option<String>,
}

impl Client {
    /// A client on machine `machine`, which it names in its hello.
    pub fn new(machine: MachineId) -> Self {
        Self {
            machine,
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
            popup: None,
            size: None,
            scroll: 0,
            sidebar_rows: None,
            notice: None,
            profiles: Vec::new(),
            default_profile: None,
        }
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

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The index of the first session the sidebar shows.
    pub fn sidebar_scroll(&self) -> usize {
        self.scroll.min(self.max_scroll())
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

    /// The latest thing the host refused, for the user.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
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
            ClientEvent::Tick { at } => match self.connection {
                Connection::Waiting { attempt, until } if at.instant() >= until => {
                    self.connect(attempt + 1)
                }
                Connection::Handshaking { attempt, since }
                    if at.instant() >= since.checked_add(HELLO_TIMEOUT).unwrap_or(since) =>
                {
                    self.attempt_failed(attempt, at.instant(), "the host didn't answer hello")
                }
                _ => Vec::new(),
            },
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
            Connection::Ready => {
                self.pending.clear();
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
        self.next_request += 1;
        let id = RequestId::Number(self.next_request.into());
        self.pending.insert(id.clone(), body.method());
        ClientCommand::Send(ClientMessage::Request(Request { id, body }))
    }

    fn notify(&self, note: ClientNotification) -> Option<ClientCommand> {
        (self.connection == Connection::Ready)
            .then_some(ClientCommand::Send(ClientMessage::Notification(note)))
    }

    fn response(&mut self, response: RawResponse) -> Vec<ClientCommand> {
        let Some(method) = self.pending.remove(&response.id) else {
            return Vec::new();
        };
        let result = match response.typed(method) {
            Ok(typed) => typed.result,
            Err(e) => Err(WireError::Internal(e.to_string())),
        };
        match (method, result) {
            (Method::Hello, Ok(HostResult::Hello(ok))) => {
                self.set_profiles(ok.profiles, ok.default_profile);
                self.connection = Connection::Ready;
                self.launched = false;
                let subscribe = Subscribe {
                    from: self.subscribe_from(),
                };
                let mut commands = vec![self.request(ClientRequest::Subscribe(subscribe))];
                commands.extend(self.resize_selected());
                commands
            }
            (Method::Hello, Err(e)) => self.fail(format!("the shepherd host refused us: {e}")),
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
                self.notice = Some(e.to_string());
                Vec::new()
            }
        }
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
        self.registry.apply(env);
        if let Some(id) = env.session() {
            self.states.entry(id.clone()).or_default().apply(env);
        }
        self.refresh()
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

    /// The selected session has been seen up to its latest entry.
    fn mark_seen(&mut self) {
        let Some(id) = self.selected.clone() else {
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
        let by = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
        let from = self.sidebar_scroll();
        let to = if rows < 0 {
            from.saturating_sub(by)
        } else {
            from.saturating_add(by)
        };
        self.scroll = to.min(self.max_scroll());
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

    // ---------- the user ----------

    fn intent(&mut self, intent: Intent) -> Vec<ClientCommand> {
        match intent {
            Intent::NextSession => self.step(true),
            Intent::PrevSession => self.step(false),
            Intent::Input(data) => {
                let Some(session) = self.selected.clone() else {
                    return Vec::new();
                };
                if self.focus != Focus::Terminal || self.popup.is_some() {
                    return Vec::new();
                }
                let note = ClientNotification::SessionInput(SessionInput { session, data });
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
            Intent::NewSession => self.new_session(None),
            Intent::BeginNewSession => match &self.profiles[..] {
                [] => self.new_session(None),
                [only] => {
                    let name = only.name.clone();
                    self.new_session(Some(name))
                }
                _ => {
                    self.popup = Some(Popup::PickProfile);
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
            Intent::BeginClose => {
                if let Some(session) = self.selected.clone() {
                    self.begin_close(session);
                }
                Vec::new()
            }
            Intent::BeginCloseSession(session) => {
                self.begin_close(session);
                Vec::new()
            }
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
                    MenuItem::Rename => self.begin_rename(session),
                    MenuItem::Close => self.begin_close(session),
                }
                Vec::new()
            }
            Intent::Confirm => match self.popup.take() {
                Some(Popup::Rename { session, name }) => {
                    self.user_request(ClientRequest::SessionRename(SessionRename {
                        session,
                        name,
                    }))
                }
                Some(Popup::ConfirmClose { session }) => {
                    self.user_request(ClientRequest::SessionClose(SessionClose { session }))
                }
                Some(Popup::Help | Popup::Menu { .. } | Popup::PickProfile) | None => Vec::new(),
            },
            Intent::Dismiss => {
                self.popup = None;
                Vec::new()
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

    /// Asks to close `session`, if it's listed.
    fn begin_close(&mut self, session: SessionId) {
        if self.listed(&session) {
            self.popup = Some(Popup::ConfirmClose { session });
        }
    }

    /// Starts a session running `profile`, or the host's default.
    fn new_session(&mut self, profile: Option<String>) -> Vec<ClientCommand> {
        self.user_request(ClientRequest::SessionNew(SessionNew { profile, cwd: None }))
    }

    /// A request the user asked for. Dropped, with a notice, while not connected.
    fn user_request(&mut self, body: ClientRequest) -> Vec<ClientCommand> {
        if self.connection != Connection::Ready {
            self.notice = Some("not connected to the shepherd host".to_owned());
            return Vec::new();
        }
        vec![self.request(body)]
    }
}
