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
use super::codec::frame;
use crate::acpp::{Envelope, RequestId, Seq};
use crate::config::{AgentKind, Config};
use crate::ids::{MachineId, SessionId};

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
}

impl Method {
    pub const ALL: [Method; 8] = [
        Self::Hello,
        Self::SessionNew,
        Self::SessionRename,
        Self::SessionClose,
        Self::Subscribe,
        Self::HookFrame,
        Self::SessionResume,
        Self::WorkspaceSelect,
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
}

/// Start a session. Answered with [`SessionNewOk`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRename {
    pub session: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClose {
    pub session: SessionId,
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
        })
    }
}

/// The host's answer to `_shepherd/hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    pub version: u32,
    pub machine: MachineId,
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
    /// The request was accepted and has nothing to return. `{}` on the wire.
    Ack,
}

impl HostResult {
    fn to_value(&self) -> Value {
        match self {
            Self::Hello(r) => json(r),
            Self::SessionNew(r) => json(r),
            Self::Ack => Value::Object(Map::new()),
        }
    }

    fn parse(method: Method, result: Value) -> Result<Self, serde_json::Error> {
        Ok(match method {
            Method::Hello => Self::Hello(serde_json::from_value(result)?),
            Method::SessionNew => Self::SessionNew(serde_json::from_value(result)?),
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
        profiles: Vec::new(),
        default_profile: None,
        notify: Notify::default(),
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
