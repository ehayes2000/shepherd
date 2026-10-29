//! The typed client↔host messages and their encode/decode functions.
//!
//! The client sends [`Request`]s. The host answers each with a [`HostResponse`] carrying the same id, and pushes
//! [`HostNotification`]s. Every encoded message is one JSON line, ready for the [`Decoder`](super::Decoder).
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
use crate::acpp::{Envelope, FrameDraft, RequestId, Seq};
use crate::ids::{MachineId, SessionId};

/// The client↔host protocol version this code speaks. Exchanged in `_shepherd/hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// A client request, as `_shepherd/*` method names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Hello,
    SessionNew,
    SessionRename,
    SessionClose,
    SessionInput,
    SessionResize,
    Subscribe,
    HookFrame,
}

impl Method {
    pub const ALL: [Method; 8] = [
        Self::Hello,
        Self::SessionNew,
        Self::SessionRename,
        Self::SessionClose,
        Self::SessionInput,
        Self::SessionResize,
        Self::Subscribe,
        Self::HookFrame,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "_shepherd/hello",
            Self::SessionNew => "_shepherd/session/new",
            Self::SessionRename => "_shepherd/session/rename",
            Self::SessionClose => "_shepherd/session/close",
            Self::SessionInput => "_shepherd/session/input",
            Self::SessionResize => "_shepherd/session/resize",
            Self::Subscribe => "_shepherd/subscribe",
            Self::HookFrame => "_shepherd/hook/frame",
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
    SessionInput(SessionInput),
    SessionResize(SessionResize),
    Subscribe(Subscribe),
    HookFrame(Box<HookFrame>),
}

/// The first request on a connection. Answered with [`HelloOk`], or [`WireError::VersionMismatch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub machine: MachineId,
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

/// Bytes typed into a session's PTY. `data` is base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInput {
    pub session: SessionId,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResize {
    pub session: SessionId,
    pub cols: u16,
    pub rows: u16,
}

/// Stream log entries, resuming each log at the given seq (see `from: seq` in `docs/acpp.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    pub from: SubscribeFrom,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeFrom {
    /// Where to resume the host log. `None` when the client has none of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Seq>,
    /// Where to resume each session log the client already has.
    #[serde(default)]
    pub sessions: BTreeMap<SessionId, Seq>,
}

/// A frame from `shepherd hook`, for the host to seal and append.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookFrame {
    pub draft: FrameDraft,
}

impl ClientRequest {
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::SessionNew(_) => Method::SessionNew,
            Self::SessionRename(_) => Method::SessionRename,
            Self::SessionClose(_) => Method::SessionClose,
            Self::SessionInput(_) => Method::SessionInput,
            Self::SessionResize(_) => Method::SessionResize,
            Self::Subscribe(_) => Method::Subscribe,
            Self::HookFrame(_) => Method::HookFrame,
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::Hello(p) => json(p),
            Self::SessionNew(p) => json(p),
            Self::SessionRename(p) => json(p),
            Self::SessionClose(p) => json(p),
            Self::SessionInput(p) => json(p),
            Self::SessionResize(p) => json(p),
            Self::Subscribe(p) => json(p),
            Self::HookFrame(p) => json(p),
        }
    }

    fn parse(method: &str, params: Value) -> Result<Self, WireError> {
        fn p<T: DeserializeOwned>(params: Value) -> Result<T, WireError> {
            serde_json::from_value(params).map_err(|e| WireError::InvalidParams(e.to_string()))
        }
        let method =
            Method::parse(method).ok_or_else(|| WireError::MethodNotFound(method.to_owned()))?;
        Ok(match method {
            Method::Hello => Self::Hello(p(params)?),
            Method::SessionNew => Self::SessionNew(p(params)?),
            Method::SessionRename => Self::SessionRename(p(params)?),
            Method::SessionClose => Self::SessionClose(p(params)?),
            Method::SessionInput => Self::SessionInput(p(params)?),
            Method::SessionResize => Self::SessionResize(p(params)?),
            Method::Subscribe => Self::Subscribe(p(params)?),
            Method::HookFrame => Self::HookFrame(p(params)?),
        })
    }
}

/// The host's answer to `_shepherd/hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    pub version: u32,
    pub machine: MachineId,
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

/// The host could not read a client's request. Answer it with [`RequestError::into_response`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{error}")]
pub struct RequestError {
    /// The request's id, when it got far enough to have one.
    pub id: Option<RequestId>,
    pub error: WireError,
}

impl RequestError {
    /// The error response. Its id is `null` when the request's id couldn't be read, as JSON-RPC 2.0 requires.
    pub fn into_response(self) -> HostResponse {
        HostResponse {
            id: self.id.unwrap_or(RequestId::Null),
            result: Err(self.error),
        }
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

/// Answers a `_shepherd/hello`: the host's version and machine, or a version mismatch.
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
    })
}

pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut obj = jsonrpc_object();
    obj.insert("id".to_owned(), json(&req.id));
    obj.insert("method".to_owned(), req.body.method().as_str().into());
    obj.insert("params".to_owned(), req.body.params());
    line(obj)
}

/// Reads one request line. On failure the error carries the request's id when it could be read, so the host
/// can answer it.
pub fn decode_request(line: &[u8]) -> Result<Request, RequestError> {
    let fail = |id: Option<RequestId>, error| RequestError { id, error };
    let invalid =
        |id: Option<RequestId>, why: &str| fail(id, WireError::InvalidRequest(why.to_owned()));
    let v: Value =
        serde_json::from_slice(line).map_err(|e| fail(None, WireError::Parse(e.to_string())))?;
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
    let id = id.ok_or_else(|| invalid(None, "client messages must be requests with an \"id\""))?;
    let body = ClientRequest::parse(method, params(obj)).map_err(|e| fail(Some(id.clone()), e))?;
    Ok(Request { id, body })
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

pub fn encode_notification(note: &HostNotification) -> Vec<u8> {
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
