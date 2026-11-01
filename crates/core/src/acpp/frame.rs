//! JSON-RPC 2.0 frames: lossless parsing, synthesized-frame marking, and response correlation.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};

use super::Dir;
use super::acp::{AcpFrame, PromptOutcome, PromptResponse};
use super::shepherd::ShepherdFrame;

/// One JSON-RPC 2.0 message from the log.
///
/// Every variant keeps the raw JSON it was parsed from, and serializing a frame writes exactly that JSON back.
/// Any well-formed JSON-RPC 2.0 message parses: a request or notification with a method shepherd doesn't type, or
/// a known method whose params don't have the expected shape, is `Unknown`.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// A request or notification in shepherd's ACP subset.
    Acp(Box<Message<AcpFrame>>),
    /// A `_shepherd/*` request or notification from the initial table.
    Shepherd(Message<ShepherdFrame>),
    /// Any response. Its meaning depends on the request it answers, see [`PendingRequests`].
    Response(Response),
    /// Any other request or notification.
    Unknown(Value),
}

/// A typed request or notification, next to its raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Message<T> {
    raw: Value,
    id: Option<RequestId>,
    body: T,
}

impl<T> Message<T> {
    pub fn body(&self) -> &T {
        &self.body
    }

    /// The request id. `None` for a notification.
    pub fn id(&self) -> Option<&RequestId> {
        self.id.as_ref()
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }
}

/// A JSON-RPC response: an id and either a `result` or an `error`, both kept raw.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    raw: Value,
    id: RequestId,
}

impl Response {
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    pub fn result(&self) -> Option<&Value> {
        self.raw.get("result")
    }

    pub fn error(&self) -> Option<&Value> {
        self.raw.get("error")
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }
}

/// A JSON-RPC request id: a string, a number, or `null`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Null,
    Number(Number),
    String(String),
}

impl RequestId {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Null => Some(Self::Null),
            Value::Number(n) => Some(Self::Number(n.clone())),
            Value::String(s) => Some(Self::String(s.clone())),
            _ => None,
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Number(n) => Value::Number(n.clone()),
            Self::String(s) => Value::String(s.clone()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Number(_) => 1,
            Self::String(_) => 2,
        }
    }
}

impl Ord for RequestId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.to_string().cmp(&b.to_string()),
            (Self::String(a), Self::String(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for RequestId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        Self::Number(n.into())
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => s.serialize_unit(),
            Self::Number(n) => n.serialize(s),
            Self::String(v) => s.serialize_str(v),
        }
    }
}

/// Why a JSON value is not a JSON-RPC 2.0 message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame is not a JSON object")]
    NotAnObject,
    #[error("frame has no \"jsonrpc\" field")]
    MissingJsonRpc,
    #[error("unsupported jsonrpc version {0}, expected \"2.0\"")]
    UnsupportedVersion(String),
    #[error("\"method\" is not a string")]
    InvalidMethod,
    #[error("\"id\" is not a string, number or null")]
    InvalidId,
    #[error("response has both \"result\" and \"error\"")]
    ResultAndError,
    #[error("response has no \"id\"")]
    MissingId,
    #[error("frame has neither \"method\" nor \"result\"/\"error\"")]
    NotAMessage,
    #[error("cannot mark as synthesized: params, result or _meta is not an object")]
    Unmarkable,
}

impl Frame {
    /// Parses a JSON-RPC 2.0 message. Only a malformed message is an error; an unknown method is `Unknown`.
    pub fn from_value(raw: Value) -> Result<Self, FrameError> {
        let obj = raw.as_object().ok_or(FrameError::NotAnObject)?;
        match obj.get("jsonrpc") {
            None => return Err(FrameError::MissingJsonRpc),
            Some(Value::String(v)) if v == "2.0" => {}
            Some(other) => return Err(FrameError::UnsupportedVersion(other.to_string())),
        }
        let id = obj
            .get("id")
            .map(|v| RequestId::from_value(v).ok_or(FrameError::InvalidId))
            .transpose()?;

        let Some(method) = obj.get("method") else {
            return match (obj.contains_key("result"), obj.contains_key("error")) {
                (true, true) => Err(FrameError::ResultAndError),
                (false, false) => Err(FrameError::NotAMessage),
                _ => {
                    let id = id.ok_or(FrameError::MissingId)?;
                    Ok(Self::Response(Response { raw, id }))
                }
            };
        };
        let method = method.as_str().ok_or(FrameError::InvalidMethod)?;
        let params = obj
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));

        if let Some(body) = AcpFrame::parse(method, id.is_some(), params.clone()) {
            return Ok(Self::Acp(Box::new(Message { raw, id, body })));
        }
        if let Some(body) = ShepherdFrame::parse(method, params) {
            return Ok(Self::Shepherd(Message { raw, id, body }));
        }
        Ok(Self::Unknown(raw))
    }

    /// Builds a request: `{"jsonrpc", "id", "method", "params"}`, in that key order.
    pub fn request(id: RequestId, method: &str, params: Value) -> Self {
        Self::built(Map::from_iter([
            jsonrpc(),
            ("id".to_owned(), id.to_value()),
            ("method".to_owned(), Value::from(method)),
            ("params".to_owned(), params),
        ]))
    }

    /// Builds a notification: `{"jsonrpc", "method", "params"}`, in that key order.
    pub fn notification(method: &str, params: Value) -> Self {
        Self::built(Map::from_iter([
            jsonrpc(),
            ("method".to_owned(), Value::from(method)),
            ("params".to_owned(), params),
        ]))
    }

    /// Builds a successful response: `{"jsonrpc", "id", "result"}`, in that key order.
    pub fn response(id: RequestId, result: Value) -> Self {
        Self::built(Map::from_iter([
            jsonrpc(),
            ("id".to_owned(), id.to_value()),
            ("result".to_owned(), result),
        ]))
    }

    /// A message the builders above assembled. It always has `"jsonrpc": "2.0"`, a string method or a result, and
    /// a valid id if any, so it always parses.
    fn built(raw: Map<String, Value>) -> Self {
        Self::from_value(Value::Object(raw))
            .unwrap_or_else(|e| unreachable!("built frame failed to parse: {e}"))
    }

    /// [`Frame::synthesized`] for a frame that is already built, e.g. by a typed builder like
    /// [`ShepherdFrame::to_frame`]. Fails only if its params, result or `_meta` is not an object.
    pub fn into_synthesized(self) -> Result<Self, FrameError> {
        Self::synthesized(self.into_raw())
    }

    /// Marks a producer-built frame as synthesized (see "Synthesized frames" in `docs/acpp.md`), then parses it.
    /// The mark is `_meta.shepherd.synthesized: true` in a request's or notification's `params` (created if
    /// absent), or in a response's `result`. Existing `_meta` keys are kept.
    pub fn synthesized(mut raw: Value) -> Result<Self, FrameError> {
        let obj = raw.as_object_mut().ok_or(FrameError::NotAnObject)?;
        let container = if obj.contains_key("method") {
            obj.entry("params")
                .or_insert_with(|| Value::Object(Map::new()))
        } else {
            obj.get_mut("result").ok_or(FrameError::Unmarkable)?
        };
        let shepherd = container
            .as_object_mut()
            .ok_or(FrameError::Unmarkable)?
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or(FrameError::Unmarkable)?
            .entry("shepherd")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or(FrameError::Unmarkable)?;
        shepherd.insert("synthesized".to_owned(), Value::Bool(true));
        Self::from_value(raw)
    }

    /// Whether a producer reconstructed this frame instead of reading it off a real ACP wire.
    pub fn is_synthesized(&self) -> bool {
        let raw = self.raw();
        let container = if raw.get("method").is_some() {
            raw.get("params")
        } else {
            raw.get("result")
        };
        container
            .and_then(|c| c.get("_meta"))
            .and_then(|m| m.get("shepherd"))
            .and_then(|s| s.get("synthesized"))
            == Some(&Value::Bool(true))
    }

    /// Whether this is a `session/prompt` a producer opened to continue work the user didn't ask for (e.g. a
    /// Claude Code turn a Stop hook kept going), marked `_meta.shepherd.continuation: true` in its `params`.
    pub fn is_continuation(&self) -> bool {
        let raw = self.raw();
        raw.get("method").and_then(Value::as_str) == Some("session/prompt")
            && raw
                .get("params")
                .and_then(|p| p.get("_meta"))
                .and_then(|m| m.get("shepherd"))
                .and_then(|s| s.get("continuation"))
                == Some(&Value::Bool(true))
    }

    /// The JSON this frame was parsed from.
    pub fn raw(&self) -> &Value {
        match self {
            Self::Acp(m) => &m.raw,
            Self::Shepherd(m) => &m.raw,
            Self::Response(r) => &r.raw,
            Self::Unknown(raw) => raw,
        }
    }

    pub fn into_raw(self) -> Value {
        match self {
            Self::Acp(m) => m.raw,
            Self::Shepherd(m) => m.raw,
            Self::Response(r) => r.raw,
            Self::Unknown(raw) => raw,
        }
    }

    /// The method of a request or notification. `None` for a response.
    pub fn method(&self) -> Option<&str> {
        self.raw().get("method").and_then(Value::as_str)
    }

    /// The id of a request or response. `None` for a notification.
    pub fn id(&self) -> Option<RequestId> {
        self.raw().get("id").and_then(RequestId::from_value)
    }
}

impl Serialize for Frame {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.raw().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Frame::from_value(Value::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

fn jsonrpc() -> (String, Value) {
    ("jsonrpc".to_owned(), Value::from("2.0"))
}

/// What a response means, given the request it answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Correlated {
    /// The response to a `session/prompt`: the turn is over.
    Prompt(PromptOutcome),
    /// The response to some other request.
    Other { method: String },
    /// No pending request has this id.
    Unmatched,
}

/// Requests still waiting for a response, so folds can interpret responses (which carry no method).
///
/// Ids are scoped by direction: each JSON-RPC peer numbers its own requests, so a `to_agent` prompt and a
/// `from_agent` permission request may share an id. A response travels the opposite way to its request.
///
/// Rule for producers: a synthesized request and its synthesized response must use opposite `dir`s. A
/// synthesized `session/prompt` is `to_agent` and its response is `from_agent`; a synthesized
/// `session/request_permission` is `from_agent` and its response is `to_agent`. A response with the same `dir`
/// as its request never correlates. `internal` requests correlate with `internal` responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingRequests {
    pending: BTreeMap<(String, RequestId), String>,
}

impl PendingRequests {
    /// Remembers a request's method. Notifications and responses are ignored.
    pub fn record(&mut self, dir: &Dir, frame: &Frame) {
        if matches!(frame, Frame::Response(_)) {
            return;
        }
        if let (Some(id), Some(method)) = (frame.id(), frame.method()) {
            self.pending
                .insert((dir.as_str().to_owned(), id), method.to_owned());
        }
    }

    /// Interprets a response by the request it answers, and forgets that request.
    pub fn correlate(&mut self, dir: &Dir, response: &Response) -> Correlated {
        let key = (dir.reversed().as_str().to_owned(), response.id.clone());
        let Some(method) = self.pending.remove(&key) else {
            return Correlated::Unmatched;
        };
        if method != "session/prompt" {
            return Correlated::Other { method };
        }
        let outcome = match (response.result(), response.error()) {
            (Some(result), _) => match PromptResponse::deserialize(result) {
                Ok(p) => PromptOutcome::Completed(p),
                Err(_) => PromptOutcome::Malformed(result.clone()),
            },
            (None, Some(error)) => PromptOutcome::Error(error.clone()),
            (None, None) => PromptOutcome::Malformed(Value::Null),
        };
        Correlated::Prompt(outcome)
    }

    /// Forgets a request that will get no response worth reading, e.g. one a fold has already inferred the
    /// outcome of. `dir` is the request's own direction. Its response, if one arrives, is then `Unmatched`.
    pub fn forget(&mut self, dir: &Dir, id: &RequestId) {
        self.pending.remove(&(dir.as_str().to_owned(), id.clone()));
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
