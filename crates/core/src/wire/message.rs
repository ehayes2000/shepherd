//! The typed client↔host messages and their encode/decode functions.
//!
//! The client sends [`Request`]s and [`ClientNotification`]s. The host answers each request with a
//! [`HostResponse`] carrying the same id, and pushes [`HostNotification`]s. Every encoded message is one JSON
//! line, ready for the [`Decoder`](super::Decoder).
//!
//! Answering rules, following JSON-RPC 2.0:
//! - Every request gets exactly one response, an error included.
//! - A notification (no `id`) is never answered. Errors on one, such as invalid params, an unknown method or
//!   an unknown session, are dropped by the host.
//! - `_shepherd/session/input` and `_shepherd/session/resize` are notifications only, because they are high
//!   volume and fire-and-forget. Sent as a request (with an `id`), either is answered with
//!   [`WireError::InvalidRequest`].
//! - A line too malformed to tell a request from a notification is answered with a `null` id.
//!
//! [`decode_client_message`] encodes these rules in [`RequestError::into_response`], which returns `None`
//! when the host must stay silent.
//!
//! These are not log frames: the log's `_shepherd/*` extension methods live in `acpp`, and none of these
//! messages is ever appended to a log.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::WireError;
use super::base64::serde_bytes;
use super::build::{BuildId, ExeDigest};
use super::codec::frame;
use crate::acpp::{Envelope, RequestId, Seq, Timestamp};
use crate::config::{AgentKind, Config};
use crate::ids::{MachineId, SessionId};
use crate::ports::worktrees::WorktreeStatus;

/// The client↔host protocol version this code speaks. Exchanged in `_shepherd/hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// A client request method. Responses are typed by it, see [`RawResponse::typed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Hello,
    SessionNew,
    SessionRename,
    SessionClose,
    Subscribe,
    HookFrame,
    SessionResume,
    WorkspaceSelect,
    WorktreeStatus,
    HostUpgrade,
    WorktreeList,
    WorktreeCleanup,
    WorktreeRemove,
    PairStart,
    PairWait,
    PairJoin,
    PeerList,
    PeerRemove,
}

impl Method {
    pub const ALL: [Method; 18] = [
        Self::Hello,
        Self::SessionNew,
        Self::SessionRename,
        Self::SessionClose,
        Self::Subscribe,
        Self::HookFrame,
        Self::SessionResume,
        Self::WorkspaceSelect,
        Self::WorktreeStatus,
        Self::HostUpgrade,
        Self::WorktreeList,
        Self::WorktreeCleanup,
        Self::WorktreeRemove,
        Self::PairStart,
        Self::PairWait,
        Self::PairJoin,
        Self::PeerList,
        Self::PeerRemove,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "_shepherd/hello",
            Self::SessionNew => "_shepherd/session/new",
            Self::SessionRename => "_shepherd/session/rename",
            Self::SessionClose => "_shepherd/session/close",
            Self::Subscribe => "_shepherd/subscribe",
            Self::HookFrame => "_shepherd/hook/frame",
            Self::SessionResume => "_shepherd/session/resume",
            Self::WorkspaceSelect => "_shepherd/workspace/select",
            Self::WorktreeStatus => "_shepherd/worktree/status",
            Self::HostUpgrade => "_shepherd/host/upgrade",
            Self::WorktreeList => "_shepherd/worktree/list",
            Self::WorktreeCleanup => "_shepherd/worktree/cleanup",
            Self::WorktreeRemove => "_shepherd/worktree/remove",
            Self::PairStart => "_shepherd/pair/start",
            Self::PairWait => "_shepherd/pair/wait",
            Self::PairJoin => "_shepherd/pair/join",
            Self::PeerList => "_shepherd/peer/list",
            Self::PeerRemove => "_shepherd/peer/remove",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// A request from a client (the TUI, or `shepherd hook`) to a host.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: RequestId,
    pub body: ClientRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequest {
    Hello(Hello),
    SessionNew(SessionNew),
    SessionRename(SessionRename),
    SessionClose(SessionClose),
    Subscribe(Subscribe),
    HookFrame(Box<HookFrame>),
    SessionResume(SessionResume),
    WorkspaceSelect(WorkspaceSelect),
    WorktreeStatus(WorktreeCheck),
    HostUpgrade(HostUpgrade),
    WorktreeList(WorktreeList),
    WorktreeCleanup(WorktreeCleanup),
    WorktreeRemove(WorktreeRemove),
    PairStart(PairStart),
    PairWait(PairWait),
    PairJoin(PairJoin),
    PeerList(PeerList),
    PeerRemove(PeerRemove),
}

/// A fire-and-forget message from a client. Never answered, see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientNotification {
    /// `_shepherd/session/input`
    SessionInput(SessionInput),
    /// `_shepherd/session/resize`
    SessionResize(SessionResize),
}

/// What a host reads from a client.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Request(Request),
    Notification(ClientNotification),
}

const SESSION_INPUT: &str = "_shepherd/session/input";
const SESSION_RESIZE: &str = "_shepherd/session/resize";

impl ClientNotification {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionInput(_) => SESSION_INPUT,
            Self::SessionResize(_) => SESSION_RESIZE,
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::SessionInput(p) => json(p),
            Self::SessionResize(p) => json(p),
        }
    }

    fn is_method(method: &str) -> bool {
        matches!(method, SESSION_INPUT | SESSION_RESIZE)
    }

    fn parse(method: &str, params: Value) -> Result<Self, WireError> {
        match method {
            SESSION_INPUT => invalid_params(params).map(Self::SessionInput),
            SESSION_RESIZE => invalid_params(params).map(Self::SessionResize),
            other => Err(WireError::MethodNotFound(other.to_owned())),
        }
    }
}

fn invalid_params<T: DeserializeOwned>(params: Value) -> Result<T, WireError> {
    serde_json::from_value(params).map_err(|e| WireError::InvalidParams(e.to_string()))
}

/// The first request on a connection. Answered with [`HelloOk`], or [`WireError::VersionMismatch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub machine: MachineId,
    /// The directory the client was started in. A host with no workspace selected yet selects it if it is in a git
    /// repo. Absent from clients older than workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The build the client runs. Absent from clients older than builds, or one that couldn't read its own
    /// executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildId>,
}

/// Start a session. Answered with [`SessionNewOk`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Where it runs. A session given one gets no new worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// With `cwd`: `cwd` is a managed worktree on this branch (one `_shepherd/worktree/list` gave), and the session
    /// runs in it as its own, recorded in its `session/created`. Ignored without `cwd`. Absent from clients older than
    /// the finder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The size of the client's terminal view, which the session's PTY spawns at (unless the client resizes it before
    /// it spawns), clamped to what the host's screens hold. Absent from older clients: the host spawns it at its default
    /// size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<ViewSize>,
}

/// A client's terminal view, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRename {
    pub session: SessionId,
    pub name: String,
}

/// Close a session. Answered with an ack once it is closing: its PTY is killed at once, and if it has a worktree, what
/// happens to it (`worktree`) is recorded in its `session/closed` once it is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClose {
    pub session: SessionId,
    /// What to do with the session's worktree, if it has one. Left off the wire for the default; clients older than
    /// worktree removal send none.
    #[serde(default, skip_serializing_if = "CloseWorktree::is_if_safe")]
    pub worktree: CloseWorktree,
}

/// What closing a session does with its worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloseWorktree {
    /// Remove it, after its pre-remove hooks, if that loses nothing (no uncommitted changes, and its commits are
    /// merged or pushed); otherwise keep it, saying why.
    #[default]
    IfSafe,
    /// Remove it, after its pre-remove hooks, even though that loses what `confirmed` says: the status the user was
    /// shown and chose to remove anyway. If the worktree has more to lose by then, it is kept.
    Remove { confirmed: WorktreeStatus },
    /// Keep it.
    Keep,
}

impl CloseWorktree {
    fn is_if_safe(&self) -> bool {
        *self == Self::IfSafe
    }
}

/// What removing a session's worktree would lose. Answered with [`WorktreeStatusOk`], or [`WireError::InvalidParams`]
/// saying why it can't be told (the session has no worktree to remove, or it couldn't be inspected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCheck {
    pub session: SessionId,
}

/// The host's answer to `_shepherd/worktree/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatusOk {
    /// `None` when the worktree is already gone: closing the session removes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WorktreeStatus>,
}

/// The current workspace's managed worktrees that no open session runs in. Answered with [`WorktreeListOk`] (empty
/// with no workspace), or [`WireError::InvalidParams`] saying why they couldn't be listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeList {}

/// The host's answer to `_shepherd/worktree/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeListOk {
    #[serde(default)]
    pub worktrees: Vec<WorktreeInfo>,
}

/// A managed worktree, as `_shepherd/worktree/list` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    /// `None` when its HEAD is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The current workspace's orphan worktrees, as `_shepherd/worktree/list` finds them, each with its status: what the
/// cleanup view lists. Answered with [`WorktreeCleanupOk`] (no worktrees with no workspace), or
/// [`WireError::InvalidParams`] saying why they couldn't be listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCleanup {}

/// The host's answer to `_shepherd/worktree/cleanup`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCleanupOk {
    #[serde(default)]
    pub worktrees: Vec<CleanupInfo>,
    /// How many days after its PTY exited a session whose branch is merged counts as stale (`cleanup_after_days` in
    /// the host's config).
    pub cleanup_after_days: u32,
}

/// An orphan worktree, as `_shepherd/worktree/cleanup` lists it: what removing it would lose, or why that can't be
/// told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupInfo {
    pub path: String,
    /// `None` when its HEAD is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WorktreeStatus>,
    /// Why its status couldn't be found, for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Remove `path`, one of the current workspace's orphan worktrees, if that loses nothing: the same inspection,
/// pre-remove hooks and removal as closing a session's worktree. Its hooks get the branch git lists for it, so the
/// client names none (any other field is ignored). Answered with [`WorktreeRemoveOk`] once that is done, or
/// [`WireError::InvalidParams`] when it can't start (no workspace, the worktree is in use or already being removed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemove {
    pub path: String,
}

/// The host's answer to `_shepherd/worktree/remove`: removed, or kept and why. Neither means it was already gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoveOk {
    pub removed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

/// Resume an exited session's agent where it left off, in the same session. Answered with an ack, or
/// [`WireError::InvalidParams`] saying why it can't be resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResume {
    pub session: SessionId,
}

/// Make `path`, a git repo on the host, the host's workspace. Answered with an ack once the selection is in the host
/// log, or [`WireError::InvalidParams`] saying why `path` can't be a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSelect {
    pub path: String,
}

/// Ask the host to update itself in place to the build now installed at its path (spike T36, decision 0018).
/// Answered with [`UpgradeAnswer`]. On [`UpgradeAnswer::Accepted`] the host closes every connection and re-execs;
/// its sessions keep running, and a reconnect finds the new build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostUpgrade {
    /// Swap even when the installed build is the running one (for tests). Left off the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// The host's answer to `_shepherd/host/upgrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "kebab-case")]
pub enum UpgradeAnswer {
    /// It already runs the installed build.
    NothingToDo,
    /// It is in the middle of something it can't carry across the swap; ask again shortly.
    Busy { reason: String },
    /// It can't update itself in place, and why: restarting it (which stops its sessions) is the way left.
    Unavailable { reason: String },
    /// It is swapping now: the connection closes, and reconnecting reaches the new build.
    Accepted,
}

/// `_shepherd/pair/start`: open a pairing on this host (`shepherd pair`), replacing any open one. Answered with
/// [`PairStartOk`] once the ticket is ready, or an error saying why there is none (the network endpoint couldn't be
/// bound). The pairing is single use and expires; `_shepherd/pair/wait` says how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStart {}

/// The host's answer to `_shepherd/pair/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStartOk {
    /// What the other machine's `shepherd join` takes.
    pub ticket: String,
    /// When the pairing stops being accepted.
    pub expires: Timestamp,
}

/// `_shepherd/pair/wait`: answered with [`PairOutcome`] once the open pairing ends, or [`WireError::InvalidParams`]
/// at once when none is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairWait {}

/// How an open pairing ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum PairOutcome {
    /// `peer` joined: each machine now trusts the other.
    Paired { peer: PeerInfo },
    /// Machine `peer`, calling itself `name`, used the ticket with the wrong token, which used it up.
    Refused { peer: MachineId, name: String },
    /// Nobody joined in time.
    Expired,
    /// A newer `_shepherd/pair/start` replaced it.
    Replaced,
}

/// `_shepherd/pair/join`: pair with the machine whose `shepherd pair` printed `ticket`. Answered with the
/// [`PeerInfo`] of the machine joined once each trusts the other, or [`WireError::InvalidParams`] saying why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairJoin {
    pub ticket: String,
}

/// `_shepherd/peer/list`: answered with [`PeerListOk`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerList {}

/// The trusted peers, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerListOk {
    pub peers: Vec<PeerInfo>,
}

/// `_shepherd/peer/remove`: stop trusting `peer`, a trusted machine's id or name (`shepherd unpair`). Answered with
/// the [`PeerInfo`] of the machine removed, or [`WireError::InvalidParams`] when no peer, or more than one, matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRemove {
    pub peer: String,
}

/// A trusted machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: MachineId,
    /// What it calls itself, display-safe.
    pub name: String,
    /// The host has a connection with it now.
    #[serde(default)]
    pub connected: bool,
}

/// Bytes typed into a session's PTY. `data` is base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInput {
    pub session: SessionId,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// What made the bytes. Keys from a client older than kinds, and left off the wire for keys.
    #[serde(default, skip_serializing_if = "InputKind::is_keys")]
    pub kind: InputKind,
}

/// What made a [`SessionInput`]'s bytes. Only some input can answer a dialog in the session
/// ([`QuietWatch`](crate::host::quiet::QuietWatch)), and the bytes alone don't always say which.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    /// A key the user pressed, encoded for the session. From a client older than [`InputKind::Answer`], any key;
    /// from a newer one, a key that can't answer a dialog.
    #[default]
    Keys,
    /// A key the user pressed that can answer a dialog (Enter, Esc, Ctrl-C or a printable key), encoded for the
    /// session. The client knows the key; its bytes may not say (kitty keyboard modes encode every key as `CSI … u`).
    Answer,
    /// Text the user pasted.
    Paste,
    /// The window gaining or losing focus (`CSI I` / `CSI O`), reported because the session asked.
    Focus,
    /// A mouse event in the session's pane, reported because the session tracks the mouse.
    Mouse,
    /// A kind from a newer client that this side doesn't know. Its bytes are still written.
    #[serde(other)]
    Other,
}

impl InputKind {
    fn is_keys(&self) -> bool {
        *self == Self::Keys
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResize {
    pub session: SessionId,
    pub cols: u16,
    pub rows: u16,
}

/// Stream log entries as `_shepherd/log/entry` notifications, resuming each log where the client left off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    pub from: SubscribeFrom,
}

/// Where to resume each log (`from: seq` in `docs/acpp.md`). Every seq is the **last entry the client already
/// has**: the host sends that log's entries with `seq > from`. A log missing here (`host: None`, or a session not
/// in `sessions`) is sent from its first entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeFrom {
    /// The last host-log entry the client has. `None`: send the whole host log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Seq>,
    /// The last entry the client has of each session log. A session not listed is sent in full.
    #[serde(default)]
    pub sessions: BTreeMap<SessionId, Seq>,
}

/// A hook payload from `shepherd hook`, exactly as the agent wrote it to the hook's stdin. The host translates it
/// with `agent`'s translator, keeping per-session translation state, and appends the frames to `session`'s log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookFrame {
    pub session: SessionId,
    pub agent: AgentKind,
    pub payload: Value,
}

impl ClientRequest {
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::SessionNew(_) => Method::SessionNew,
            Self::SessionRename(_) => Method::SessionRename,
            Self::SessionClose(_) => Method::SessionClose,
            Self::Subscribe(_) => Method::Subscribe,
            Self::HookFrame(_) => Method::HookFrame,
            Self::SessionResume(_) => Method::SessionResume,
            Self::WorkspaceSelect(_) => Method::WorkspaceSelect,
            Self::WorktreeStatus(_) => Method::WorktreeStatus,
            Self::HostUpgrade(_) => Method::HostUpgrade,
            Self::WorktreeList(_) => Method::WorktreeList,
            Self::WorktreeCleanup(_) => Method::WorktreeCleanup,
            Self::WorktreeRemove(_) => Method::WorktreeRemove,
            Self::PairStart(_) => Method::PairStart,
            Self::PairWait(_) => Method::PairWait,
            Self::PairJoin(_) => Method::PairJoin,
            Self::PeerList(_) => Method::PeerList,
            Self::PeerRemove(_) => Method::PeerRemove,
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::Hello(p) => json(p),
            Self::SessionNew(p) => json(p),
            Self::SessionRename(p) => json(p),
            Self::SessionClose(p) => json(p),
            Self::Subscribe(p) => json(p),
            Self::HookFrame(p) => json(p),
            Self::SessionResume(p) => json(p),
            Self::WorkspaceSelect(p) => json(p),
            Self::WorktreeStatus(p) => json(p),
            Self::HostUpgrade(p) => json(p),
            Self::WorktreeList(p) => json(p),
            Self::WorktreeCleanup(p) => json(p),
            Self::WorktreeRemove(p) => json(p),
            Self::PairStart(p) => json(p),
            Self::PairWait(p) => json(p),
            Self::PairJoin(p) => json(p),
            Self::PeerList(p) => json(p),
            Self::PeerRemove(p) => json(p),
        }
    }

    fn parse(method: &str, params: Value) -> Result<Self, WireError> {
        let method =
            Method::parse(method).ok_or_else(|| WireError::MethodNotFound(method.to_owned()))?;
        Ok(match method {
            Method::Hello => Self::Hello(invalid_params(params)?),
            Method::SessionNew => Self::SessionNew(invalid_params(params)?),
            Method::SessionRename => Self::SessionRename(invalid_params(params)?),
            Method::SessionClose => Self::SessionClose(invalid_params(params)?),
            Method::Subscribe => Self::Subscribe(invalid_params(params)?),
            Method::HookFrame => Self::HookFrame(invalid_params(params)?),
            Method::SessionResume => Self::SessionResume(invalid_params(params)?),
            Method::WorkspaceSelect => Self::WorkspaceSelect(invalid_params(params)?),
            Method::WorktreeStatus => Self::WorktreeStatus(invalid_params(params)?),
            Method::HostUpgrade => Self::HostUpgrade(invalid_params(params)?),
            Method::WorktreeList => Self::WorktreeList(invalid_params(params)?),
            Method::WorktreeCleanup => Self::WorktreeCleanup(invalid_params(params)?),
            Method::WorktreeRemove => Self::WorktreeRemove(invalid_params(params)?),
            Method::PairStart => Self::PairStart(invalid_params(params)?),
            Method::PairWait => Self::PairWait(invalid_params(params)?),
            Method::PairJoin => Self::PairJoin(invalid_params(params)?),
            Method::PeerList => Self::PeerList(invalid_params(params)?),
            Method::PeerRemove => Self::PeerRemove(invalid_params(params)?),
        })
    }
}

/// The host's answer to `_shepherd/hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    pub version: u32,
    pub machine: MachineId,
    /// What the host's machine is called (see [`crate::machine`]). `None` from a host older than machine names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The agent profiles new sessions can run, from the host's config. Empty from a host older than profiles.
    #[serde(default)]
    pub profiles: Vec<ProfileInfo>,
    /// The profile a session without one runs. `None` from a host older than profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// How the client tells the user a session wants them, from the host's config (the client reads none). Off from
    /// a host older than notify, and left off the wire while off.
    #[serde(default, skip_serializing_if = "Notify::is_off")]
    pub notify: Notify,
    /// The build the host runs. `None` from a host older than builds (spike T36's "host older" row), or one that
    /// couldn't read its own executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildId>,
    /// The digest of the file now at the path the host's executable was started from: what restarting it would run.
    /// `None` from a host older than builds, or when that file can't be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<ExeDigest>,
    /// Something the user should know about the host, e.g. that it couldn't read its config when it updated itself in
    /// place and runs with the defaults. Absent when there is nothing, and from older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `notify` in [`HelloOk`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notify {
    /// Ring the terminal bell (and send an OSC 9 notification) when a session starts wanting the user.
    #[serde(default)]
    pub bell: bool,
}

impl Notify {
    fn is_off(&self) -> bool {
        *self == Self::default()
    }
}

impl HelloOk {
    /// This answer, listing `config`'s profiles by name and naming its default.
    pub fn with_profiles(self, config: &Config) -> Self {
        Self {
            profiles: config
                .profiles()
                .map(|p| ProfileInfo {
                    name: p.name.clone(),
                    agent: p.kind,
                })
                .collect(),
            default_profile: Some(config.default_agent().name.clone()),
            ..self
        }
    }

    /// This answer, saying which build the host runs and what is installed at its executable's path.
    pub fn with_build(self, build: Option<BuildId>, installed: Option<ExeDigest>) -> Self {
        Self {
            build,
            installed,
            ..self
        }
    }

    /// This answer, saying what the host's machine is called.
    pub fn with_machine_name(self, name: Option<String>) -> Self {
        Self { name, ..self }
    }

    /// This answer, with `warning` for the user, if any.
    pub fn with_warning(self, warning: Option<String>) -> Self {
        Self { warning, ..self }
    }

    /// This answer, carrying `config`'s `[notify]` settings.
    pub fn with_notify(self, config: &Config) -> Self {
        Self {
            notify: Notify {
                bell: config.notify_bell(),
            },
            ..self
        }
    }
}

/// An agent profile as a client sees it: what to call it, and which agent it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub agent: AgentKind,
}

/// The host's answer to `_shepherd/session/new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewOk {
    pub session: SessionId,
}

/// A successful result, by the method it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResult {
    Hello(HelloOk),
    SessionNew(SessionNewOk),
    WorktreeStatus(WorktreeStatusOk),
    Upgrade(UpgradeAnswer),
    WorktreeList(WorktreeListOk),
    WorktreeCleanup(WorktreeCleanupOk),
    WorktreeRemove(WorktreeRemoveOk),
    PairStart(PairStartOk),
    PairWait(PairOutcome),
    PairJoin(PeerInfo),
    PeerList(PeerListOk),
    PeerRemove(PeerInfo),
    /// The request was accepted and has nothing to return. `{}` on the wire.
    Ack,
}

impl HostResult {
    fn to_value(&self) -> Value {
        match self {
            Self::Hello(r) => json(r),
            Self::SessionNew(r) => json(r),
            Self::WorktreeStatus(r) => json(r),
            Self::Upgrade(r) => json(r),
            Self::WorktreeList(r) => json(r),
            Self::WorktreeCleanup(r) => json(r),
            Self::WorktreeRemove(r) => json(r),
            Self::PairStart(r) => json(r),
            Self::PairWait(r) => json(r),
            Self::PairJoin(r) => json(r),
            Self::PeerList(r) => json(r),
            Self::PeerRemove(r) => json(r),
            Self::Ack => Value::Object(Map::new()),
        }
    }

    fn parse(method: Method, result: Value) -> Result<Self, serde_json::Error> {
        Ok(match method {
            Method::Hello => Self::Hello(serde_json::from_value(result)?),
            Method::SessionNew => Self::SessionNew(serde_json::from_value(result)?),
            Method::WorktreeStatus => Self::WorktreeStatus(serde_json::from_value(result)?),
            Method::HostUpgrade => Self::Upgrade(serde_json::from_value(result)?),
            Method::WorktreeList => Self::WorktreeList(serde_json::from_value(result)?),
            Method::WorktreeCleanup => Self::WorktreeCleanup(serde_json::from_value(result)?),
            Method::WorktreeRemove => Self::WorktreeRemove(serde_json::from_value(result)?),
            Method::PairStart => Self::PairStart(serde_json::from_value(result)?),
            Method::PairWait => Self::PairWait(serde_json::from_value(result)?),
            Method::PairJoin => Self::PairJoin(serde_json::from_value(result)?),
            Method::PeerList => Self::PeerList(serde_json::from_value(result)?),
            Method::PeerRemove => Self::PeerRemove(serde_json::from_value(result)?),
            _ => Self::Ack,
        })
    }
}

/// The host's answer to one [`Request`], with the same id.
#[derive(Debug, Clone, PartialEq)]
pub struct HostResponse {
    pub id: RequestId,
    pub result: Result<HostResult, WireError>,
}

/// A response as read off the wire, before the client matches it to the request it sent. Responses carry no
/// method, so [`RawResponse::typed`] needs it from the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct RawResponse {
    pub id: RequestId,
    pub result: Result<Value, WireError>,
}

impl RawResponse {
    pub fn typed(self, method: Method) -> Result<HostResponse, DecodeError> {
        let result = match self.result {
            Ok(v) => Ok(
                HostResult::parse(method, v).map_err(|e| DecodeError::InvalidResult {
                    method,
                    reason: e.to_string(),
                })?,
            ),
            Err(e) => Err(e),
        };
        Ok(HostResponse {
            id: self.id,
            result,
        })
    }
}

/// A message the host pushes without being asked.
#[derive(Debug, Clone, PartialEq)]
pub enum HostNotification {
    /// `_shepherd/log/entry`: an entry appended to a log the client subscribed to.
    LogEntry(Box<LogEntry>),
    /// `_shepherd/pty/output`: bytes a session's PTY wrote. Never logged.
    PtyOutput(PtyOutput),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub envelope: Envelope,
}

/// `bytes` is base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyOutput {
    pub session: SessionId,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

const LOG_ENTRY: &str = "_shepherd/log/entry";
const PTY_OUTPUT: &str = "_shepherd/pty/output";

impl HostNotification {
    pub fn method(&self) -> &'static str {
        match self {
            Self::LogEntry(_) => LOG_ENTRY,
            Self::PtyOutput(_) => PTY_OUTPUT,
        }
    }
}

/// What a client reads from a host.
#[derive(Debug, Clone, PartialEq)]
pub enum HostMessage {
    Response(RawResponse),
    Notification(HostNotification),
}

/// The host could not read or accept a client's message. See [`RequestError::into_response`] for whether to
/// answer it.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{error}")]
pub struct RequestError {
    /// The request's id, when it got far enough to have one.
    pub id: Option<RequestId>,
    pub error: WireError,
    /// The message was a well-formed notification, so it must not be answered.
    pub notification: bool,
}

impl RequestError {
    /// The error response, or `None` for a notification, which is never answered. The id is `null` when the
    /// message's id couldn't be read, as JSON-RPC 2.0 requires.
    pub fn into_response(self) -> Option<HostResponse> {
        (!self.notification).then(|| HostResponse {
            id: self.id.unwrap_or(RequestId::Null),
            result: Err(self.error),
        })
    }
}

/// A client could not read a host's message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("not a JSON-RPC 2.0 message: {0}")]
    NotJsonRpc(String),
    /// A notification this version doesn't know. Safe to skip.
    #[error("unknown notification {0}")]
    UnknownMethod(String),
    #[error("invalid params for {method}: {reason}")]
    InvalidParams { method: String, reason: String },
    #[error("invalid result for {}: {reason}", method.as_str())]
    InvalidResult { method: Method, reason: String },
}

/// Answers a `_shepherd/hello`: the host's version and machine, or a version mismatch. The host adds its profiles
/// with [`HelloOk::with_profiles`].
pub fn negotiate(hello: &Hello, host_machine: &MachineId) -> Result<HelloOk, WireError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(WireError::VersionMismatch {
            host: PROTOCOL_VERSION,
            client: hello.version,
        });
    }
    Ok(HelloOk {
        version: PROTOCOL_VERSION,
        machine: host_machine.clone(),
        name: None,
        profiles: Vec::new(),
        default_profile: None,
        notify: Notify::default(),
        build: None,
        installed: None,
        warning: None,
    })
}

pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut obj = jsonrpc_object();
    obj.insert("id".to_owned(), json(&req.id));
    obj.insert("method".to_owned(), req.body.method().as_str().into());
    obj.insert("params".to_owned(), req.body.params());
    line(obj)
}

pub fn encode_client_notification(note: &ClientNotification) -> Vec<u8> {
    let mut obj = jsonrpc_object();
    obj.insert("method".to_owned(), note.method().into());
    obj.insert("params".to_owned(), note.params());
    line(obj)
}

/// Reads one line from a client: a request or a notification. On failure, [`RequestError::into_response`] says
/// how the host answers, following the rules in the module docs.
pub fn decode_client_message(line: &[u8]) -> Result<ClientMessage, RequestError> {
    let answered = |id: Option<RequestId>, error| RequestError {
        id,
        error,
        notification: false,
    };
    let invalid =
        |id: Option<RequestId>, why: &str| answered(id, WireError::InvalidRequest(why.to_owned()));
    let v: Value = serde_json::from_slice(line)
        .map_err(|e| answered(None, WireError::Parse(e.to_string())))?;
    let obj = v
        .as_object()
        .ok_or_else(|| invalid(None, "not an object"))?;
    let id = match obj.get("id") {
        None => None,
        Some(id) => Some(
            request_id(id)
                .ok_or_else(|| invalid(None, "\"id\" is not a string, number or null"))?,
        ),
    };
    if !is_jsonrpc(obj) {
        return Err(invalid(id, "\"jsonrpc\" must be \"2.0\""));
    }
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(id.clone(), "\"method\" must be a string"))?;

    let Some(id) = id else {
        // A well-formed notification: from here on, errors are dropped, never answered.
        let silent = |error| RequestError {
            id: None,
            error,
            notification: true,
        };
        if Method::parse(method).is_some() {
            return Err(silent(WireError::InvalidRequest(format!(
                "{method} is a request and needs an \"id\""
            ))));
        }
        return ClientNotification::parse(method, params(obj))
            .map(ClientMessage::Notification)
            .map_err(silent);
    };
    if ClientNotification::is_method(method) {
        return Err(invalid(
            Some(id),
            &format!("{method} is a notification and must not have an \"id\""),
        ));
    }
    let body =
        ClientRequest::parse(method, params(obj)).map_err(|e| answered(Some(id.clone()), e))?;
    Ok(ClientMessage::Request(Request { id, body }))
}

pub fn encode_response(resp: &HostResponse) -> Vec<u8> {
    let mut obj = jsonrpc_object();
    obj.insert("id".to_owned(), json(&resp.id));
    match &resp.result {
        Ok(result) => obj.insert("result".to_owned(), result.to_value()),
        Err(error) => obj.insert("error".to_owned(), error.to_value()),
    };
    line(obj)
}

pub fn encode_host_notification(note: &HostNotification) -> Vec<u8> {
    let params = match note {
        HostNotification::LogEntry(p) => json(p),
        HostNotification::PtyOutput(p) => json(p),
    };
    let mut obj = jsonrpc_object();
    obj.insert("method".to_owned(), note.method().into());
    obj.insert("params".to_owned(), params);
    line(obj)
}

/// Reads one line from a host: a response (typed afterwards, see [`RawResponse::typed`]) or a notification.
pub fn decode_host_message(line: &[u8]) -> Result<HostMessage, DecodeError> {
    let not_rpc = |why: &str| DecodeError::NotJsonRpc(why.to_owned());
    let v: Value = serde_json::from_slice(line).map_err(|e| DecodeError::Json(e.to_string()))?;
    let obj = v.as_object().ok_or_else(|| not_rpc("not an object"))?;
    if !is_jsonrpc(obj) {
        return Err(not_rpc("\"jsonrpc\" must be \"2.0\""));
    }
    if let Some(method) = obj.get("method") {
        let method = method
            .as_str()
            .ok_or_else(|| not_rpc("\"method\" must be a string"))?;
        return HostNotification::parse(method, params(obj)).map(HostMessage::Notification);
    }
    let id = obj
        .get("id")
        .and_then(request_id)
        .ok_or_else(|| not_rpc("response has no valid \"id\""))?;
    let result =
        match (obj.get("result"), obj.get("error")) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) => Err(WireError::from_value(error)
                .ok_or_else(|| not_rpc("malformed \"error\" object"))?),
            _ => {
                return Err(not_rpc(
                    "response needs exactly one of \"result\" and \"error\"",
                ));
            }
        };
    Ok(HostMessage::Response(RawResponse { id, result }))
}

impl HostNotification {
    fn parse(method: &str, params: Value) -> Result<Self, DecodeError> {
        fn p<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, DecodeError> {
            serde_json::from_value(params).map_err(|e| DecodeError::InvalidParams {
                method: method.to_owned(),
                reason: e.to_string(),
            })
        }
        match method {
            LOG_ENTRY => p(method, params).map(Self::LogEntry),
            PTY_OUTPUT => p(method, params).map(Self::PtyOutput),
            other => Err(DecodeError::UnknownMethod(other.to_owned())),
        }
    }
}

fn jsonrpc_object() -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("jsonrpc".to_owned(), "2.0".into());
    obj
}

fn is_jsonrpc(obj: &Map<String, Value>) -> bool {
    obj.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
}

/// A message's params, `{}` when absent.
fn params(obj: &Map<String, Value>) -> Value {
    obj.get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn request_id(v: &Value) -> Option<RequestId> {
    match v {
        Value::Null => Some(RequestId::Null),
        Value::Number(n) => Some(RequestId::Number(n.clone())),
        Value::String(s) => Some(RequestId::String(s.clone())),
        _ => None,
    }
}

/// Wire types hold only strings, numbers, string-keyed maps and already-parsed JSON, which always serialize.
pub(crate) fn json<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or_else(|e| unreachable!("wire type failed to serialize: {e}"))
}

fn line(obj: Map<String, Value>) -> Vec<u8> {
    frame(Value::Object(obj).to_string().into_bytes())
}
