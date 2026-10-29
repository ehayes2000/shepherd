//! Shepherd's typed subset of the Agent Client Protocol, pinned to **ACP protocol v1 / schema 1.13.0**
//! (<https://agentclientprotocol.com/protocol/v1>). See decision 0006 and `docs/spikes/T01-acp-types.md` §5.
//!
//! These are views. Only the fields the folds read are typed, everything else stays in the frame's raw JSON.
//! Producers build frames from them with the `to_frame` methods, which write fields in declaration order and skip
//! absent optional fields. A frame built from a value parses back to an equal value, with one exception: an
//! optional raw field (`_meta`, `rawInput`, `rawOutput`) holding `Some(Value::Null)` is written as `null` and reads
//! back as `None`. Every enum is open, so a value from a newer ACP still parses. Upgrading the pin is a PM decision.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::shepherd::to_params;
use super::{Frame, RequestId};

/// The ACP methods shepherd understands. Which one a frame is depends on its method and whether it has an id.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpFrame {
    /// `session/prompt` request.
    Prompt(PromptRequest),
    /// `session/update` notification.
    Update(SessionNotification),
    /// `session/request_permission` request.
    RequestPermission(RequestPermissionRequest),
    /// `session/cancel` notification.
    Cancel(CancelNotification),
}

impl AcpFrame {
    /// Parses the params of an ACP method. `None` if the method isn't in the subset, isn't the right kind of
    /// message, or its params don't have the expected shape.
    pub(crate) fn parse(method: &str, is_request: bool, params: Value) -> Option<Self> {
        let frame = match (method, is_request) {
            ("session/prompt", true) => Self::Prompt(serde_json::from_value(params).ok()?),
            ("session/update", false) => Self::Update(serde_json::from_value(params).ok()?),
            ("session/request_permission", true) => {
                Self::RequestPermission(serde_json::from_value(params).ok()?)
            }
            ("session/cancel", false) => Self::Cancel(serde_json::from_value(params).ok()?),
            _ => return None,
        };
        Some(frame)
    }
}

/// `session/prompt` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// The agent's session id (not a shepherd [`SessionId`](crate::ids::SessionId)).
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl PromptRequest {
    /// Builds the `session/prompt` request with id `id`.
    pub fn to_frame(&self, id: RequestId) -> Frame {
        Frame::request(id, "session/prompt", to_params(self))
    }
}

/// The `result` of a `session/prompt` response. Read it with [`PendingRequests`](super::PendingRequests).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl PromptResponse {
    /// Builds the successful response to the `session/prompt` request `id`.
    pub fn to_frame(&self, id: RequestId) -> Frame {
        Frame::response(id, to_params(self))
    }
}

/// How a `session/prompt` request ended. Every outcome ends the turn.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptOutcome {
    Completed(PromptResponse),
    /// The response's `error` object.
    Error(Value),
    /// A `result` without a readable `stopReason`.
    Malformed(Value),
}

open_enum! {
    /// Why a turn ended.
    pub enum StopReason {
        EndTurn = "end_turn",
        MaxTokens = "max_tokens",
        MaxTurnRequests = "max_turn_requests",
        Refusal = "refusal",
        Cancelled = "cancelled",
    }
}

/// `session/update` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl SessionNotification {
    /// Builds the `session/update` notification.
    pub fn to_frame(&self) -> Frame {
        Frame::notification("session/update", to_params(self))
    }
}

/// The `update` of a `session/update`, keyed by its `sessionUpdate` tag.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    CompactionUpdate(CompactionUpdate),
    /// Any other `sessionUpdate`, including ACP ones shepherd doesn't read yet, or a known one whose fields don't
    /// parse.
    Other {
        kind: String,
        raw: Value,
    },
}

impl<'de> Deserialize<'de> for SessionUpdate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        let Some(kind) = raw.get("sessionUpdate").and_then(Value::as_str) else {
            return Err(serde::de::Error::missing_field("sessionUpdate"));
        };
        let kind = kind.to_owned();
        let typed = match kind.as_str() {
            "user_message_chunk" => serde_json::from_value(raw.clone()).map(Self::UserMessageChunk),
            "tool_call" => serde_json::from_value(raw.clone()).map(Self::ToolCall),
            "tool_call_update" => serde_json::from_value(raw.clone()).map(Self::ToolCallUpdate),
            "compaction_update" => serde_json::from_value(raw.clone()).map(Self::CompactionUpdate),
            _ => return Ok(Self::Other { kind, raw }),
        };
        Ok(typed.unwrap_or(Self::Other { kind, raw }))
    }
}

/// Writes the `sessionUpdate` tag first, then the variant's fields. `Other` writes its `raw` verbatim.
impl Serialize for SessionUpdate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "sessionUpdate", rename_all = "snake_case")]
        enum Tagged<'a> {
            UserMessageChunk(&'a ContentChunk),
            ToolCall(&'a ToolCall),
            ToolCallUpdate(&'a ToolCallUpdate),
            CompactionUpdate(&'a CompactionUpdate),
        }
        match self {
            Self::UserMessageChunk(c) => Tagged::UserMessageChunk(c).serialize(s),
            Self::ToolCall(c) => Tagged::ToolCall(c).serialize(s),
            Self::ToolCallUpdate(c) => Tagged::ToolCallUpdate(c).serialize(s),
            Self::CompactionUpdate(c) => Tagged::CompactionUpdate(c).serialize(s),
            Self::Other { raw, .. } => raw.serialize(s),
        }
    }
}

/// `user_message_chunk` (and, later, the agent message and thought chunks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// A new tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub status: ToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// A patch to a tool call. Also the `toolCall` of a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

open_enum! {
    /// What kind of tool is running. ACP's own `"other"` has no variant: it parses as `Other("other")`.
    pub enum ToolKind {
        Read = "read",
        Edit = "edit",
        Delete = "delete",
        Move = "move",
        Search = "search",
        Execute = "execute",
        Think = "think",
        Fetch = "fetch",
        SwitchMode = "switch_mode",
    }
}

impl Default for ToolKind {
    /// ACP's default when `kind` is absent.
    fn default() -> Self {
        Self::Other("other".to_owned())
    }
}

open_enum! {
    pub enum ToolCallStatus {
        Pending = "pending",
        InProgress = "in_progress",
        Completed = "completed",
        Failed = "failed",
    }
}

impl Default for ToolCallStatus {
    /// ACP's default when `status` is absent.
    fn default() -> Self {
        Self::Pending
    }
}

/// `compaction_update`: an upsert keyed by `compaction_id`. `summary` and `error` stay raw and have patch
/// semantics: `None` means absent (unchanged), `Some(Value::Null)` means cleared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionUpdate {
    pub compaction_id: String,
    pub status: CompactionStatus,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub summary: Option<Value>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<Value>,
}

/// Deserializes a field that is present, even as `null`, into `Some`. Absent fields fall back to `None`.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

open_enum! {
    pub enum CompactionStatus {
        InProgress = "in_progress",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
    }
}

/// `session/request_permission` params. `options` stays raw: hooks never tell us which one the user picked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub options: Value,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl RequestPermissionRequest {
    /// Builds the `session/request_permission` request with id `id`.
    pub fn to_frame(&self, id: RequestId) -> Frame {
        Frame::request(id, "session/request_permission", to_params(self))
    }
}

/// `session/cancel` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: String,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl CancelNotification {
    /// Builds the `session/cancel` notification.
    pub fn to_frame(&self) -> Frame {
        Frame::notification("session/cancel", to_params(self))
    }
}

/// A content block. Only `text` is typed.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(TextContent),
    /// Any other block (image, audio, resource, a future type), kept verbatim.
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl ContentBlock {
    /// A text block with no `_meta`.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent {
            text: text.into(),
            meta: None,
        })
    }
}

/// Writes `"type": "text"` first, then the text's fields. `Other` writes its raw block verbatim.
impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Tagged<'a> {
            Text(&'a TextContent),
        }
        match self {
            Self::Text(t) => Tagged::Text(t).serialize(s),
            Self::Other(raw) => raw.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        if raw.get("type").and_then(Value::as_str) == Some("text")
            && let Ok(text) = serde_json::from_value(raw.clone())
        {
            return Ok(Self::Text(text));
        }
        Ok(Self::Other(raw))
    }
}
