//! Errors a host returns to a client, as JSON-RPC `error` objects with stable codes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ids::SessionId;

/// Stable JSON-RPC error codes. The negative ones are JSON-RPC 2.0's own; shepherd's start at 1000.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const VERSION_MISMATCH: i64 = 1000;
    pub const UNKNOWN_SESSION: i64 = 1001;
}

/// A request failed. On the wire: `{ code, message, data? }`, with the variant's fields in `data`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WireError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// The client's `_shepherd/hello` named a protocol version the host doesn't speak.
    #[error("protocol version mismatch: host speaks {host}, client sent {client}")]
    VersionMismatch { host: u32, client: u32 },
    #[error("unknown session {session}")]
    UnknownSession { session: SessionId },
    /// A code this version doesn't know, kept as received.
    #[error("error {code}: {message}")]
    Other {
        code: i64,
        message: String,
        data: Option<Box<Value>>,
    },
}

#[derive(Serialize, Deserialize)]
struct VersionData {
    host: u32,
    client: u32,
}

#[derive(Serialize, Deserialize)]
struct SessionData {
    session: SessionId,
}

impl WireError {
    pub fn code(&self) -> i64 {
        match self {
            Self::Parse(_) => code::PARSE_ERROR,
            Self::InvalidRequest(_) => code::INVALID_REQUEST,
            Self::MethodNotFound(_) => code::METHOD_NOT_FOUND,
            Self::InvalidParams(_) => code::INVALID_PARAMS,
            Self::Internal(_) => code::INTERNAL_ERROR,
            Self::VersionMismatch { .. } => code::VERSION_MISMATCH,
            Self::UnknownSession { .. } => code::UNKNOWN_SESSION,
            Self::Other { code, .. } => *code,
        }
    }

    /// The JSON-RPC `error` object.
    pub(crate) fn to_value(&self) -> Value {
        let data = match self {
            Self::Parse(d)
            | Self::InvalidRequest(d)
            | Self::MethodNotFound(d)
            | Self::InvalidParams(d)
            | Self::Internal(d) => Some(Value::String(d.clone())),
            Self::VersionMismatch { host, client } => Some(super::message::json(&VersionData {
                host: *host,
                client: *client,
            })),
            Self::UnknownSession { session } => Some(super::message::json(&SessionData {
                session: session.clone(),
            })),
            Self::Other { data, .. } => data.as_deref().cloned(),
        };
        let message = match self {
            Self::Other { message, .. } => message.clone(),
            other => other.to_string(),
        };
        let mut obj = Map::new();
        obj.insert("code".to_owned(), self.code().into());
        obj.insert("message".to_owned(), message.into());
        if let Some(data) = data {
            obj.insert("data".to_owned(), data);
        }
        Value::Object(obj)
    }

    /// Reads a JSON-RPC `error` object. `None` if it has no integer `code` or string `message`. A known code
    /// whose `data` has the wrong shape is kept as `Other`.
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        let code = v.get("code")?.as_i64()?;
        let message = v.get("message")?.as_str()?.to_owned();
        let data = v.get("data").cloned();
        let detail = || match &data {
            Some(Value::String(d)) => d.clone(),
            _ => message.clone(),
        };
        let typed = match code {
            code::PARSE_ERROR => Some(Self::Parse(detail())),
            code::INVALID_REQUEST => Some(Self::InvalidRequest(detail())),
            code::METHOD_NOT_FOUND => Some(Self::MethodNotFound(detail())),
            code::INVALID_PARAMS => Some(Self::InvalidParams(detail())),
            code::INTERNAL_ERROR => Some(Self::Internal(detail())),
            code::VERSION_MISMATCH => data
                .clone()
                .and_then(|d| serde_json::from_value::<VersionData>(d).ok())
                .map(|d| Self::VersionMismatch {
                    host: d.host,
                    client: d.client,
                }),
            code::UNKNOWN_SESSION => data
                .clone()
                .and_then(|d| serde_json::from_value::<SessionData>(d).ok())
                .map(|d| Self::UnknownSession { session: d.session }),
            _ => None,
        };
        Some(typed.unwrap_or(Self::Other {
            code,
            message,
            data: data.map(Box::new),
        }))
    }
}
