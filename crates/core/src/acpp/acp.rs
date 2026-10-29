//! Shepherd's typed subset of the Agent Client Protocol, pinned to **ACP protocol v1 / schema 1.13.0**
//! (<https://agentclientprotocol.com/protocol/v1>). See decision 0006 and `docs/spikes/T01-acp-types.md` §5.
//!
//! These are read-only views. Only the fields the folds read are typed, everything else stays in the frame's raw
//! JSON. Every enum is open, so a value from a newer ACP still parses. Upgrading the pin is a PM decision.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// The agent's session id (not a shepherd [`SessionId`](crate::ids::SessionId)).
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// The `result` of a `session/prompt` response. Read it with [`PendingRequests`](super::PendingRequests).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
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

/// `user_message_chunk` (and, later, the agent message and thought chunks).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// A new tool call.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub status: ToolCallStatus,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// A patch to a tool call. Also the `toolCall` of a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<ToolKind>,
    #[serde(default)]
    pub status: Option<ToolCallStatus>,
    #[serde(default, rename = "_meta")]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionUpdate {
    pub compaction_id: String,
    pub status: CompactionStatus,
    #[serde(default, deserialize_with = "present")]
    pub summary: Option<Value>,
    #[serde(default, deserialize_with = "present")]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    #[serde(default)]
    pub options: Value,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// `session/cancel` params.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: String,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// A content block. Only `text` is typed.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(TextContent),
    /// Any other block (image, audio, resource, a future type), kept verbatim.
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
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
