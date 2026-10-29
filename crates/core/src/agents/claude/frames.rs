//! The JSON frames the Claude Code translator produces, built from `json!` values. Kept apart so typed frame
//! builders (F03) can replace this module without touching the mapping.
//!
//! Each builder returns `None` only if the frame it built doesn't parse, which would be a bug here. The caller
//! then falls back to a vendor frame instead of panicking.

use serde_json::{Map, Value, json};

use crate::acpp::{Frame, RequestId};

/// A `session/prompt` request carrying the prompt text, if any. A `continuation` (work a Stop hook kept going, not
/// a user prompt) is marked `_meta.shepherd.continuation: true`.
pub(super) fn prompt(
    id: &RequestId,
    agent_session: &str,
    text: Option<&str>,
    continuation: bool,
) -> Option<Frame> {
    let prompt: Vec<Value> = text.map(text_block).into_iter().collect();
    let mut params = json!({ "sessionId": agent_session, "prompt": prompt });
    if let (true, Some(obj)) = (continuation, params.as_object_mut()) {
        obj.insert(
            "_meta".to_owned(),
            json!({ "shepherd": { "continuation": true } }),
        );
    }
    synthesized(json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params }))
}

/// The `session/prompt` response that ends a turn normally.
pub(super) fn prompt_end_turn(id: &RequestId) -> Option<Frame> {
    synthesized(json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } }))
}

pub(super) fn user_message_chunk(agent_session: &str, text: &str) -> Option<Frame> {
    update(
        agent_session,
        json!({ "sessionUpdate": "user_message_chunk", "content": text_block(text) }),
    )
}

/// A new tool call, already running.
pub(super) fn tool_call(
    agent_session: &str,
    tool_call_id: &str,
    title: &str,
    kind: &str,
    raw_input: Option<&Value>,
) -> Option<Frame> {
    let mut call = json!({
        "sessionUpdate": "tool_call", "toolCallId": tool_call_id, "title": title, "kind": kind,
        "status": "in_progress",
    });
    if let (Some(obj), Some(input)) = (call.as_object_mut(), raw_input) {
        obj.insert("rawInput".to_owned(), input.clone());
    }
    update(agent_session, call)
}

/// A tool call that finished with `status` (`completed` or `failed`).
pub(super) fn tool_call_done(
    agent_session: &str,
    tool_call_id: &str,
    status: &str,
    raw_output: Option<&Value>,
) -> Option<Frame> {
    let mut patch = json!({
        "sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": status,
    });
    if let (Some(obj), Some(output)) = (patch.as_object_mut(), raw_output) {
        obj.insert("rawOutput".to_owned(), output.clone());
    }
    update(agent_session, patch)
}

/// A permission prompt. The options stay empty: hooks don't say what the dialog offers.
pub(super) fn request_permission(
    id: &RequestId,
    agent_session: &str,
    tool_call_id: &str,
    title: &str,
) -> Option<Frame> {
    synthesized(json!({
        "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {
            "sessionId": agent_session,
            "toolCall": { "toolCallId": tool_call_id, "title": title },
            "options": [],
        },
    }))
}

pub(super) fn compaction(
    agent_session: &str,
    compaction_id: &str,
    status: &str,
    summary: Option<&str>,
) -> Option<Frame> {
    let mut patch = json!({
        "sessionUpdate": "compaction_update", "compactionId": compaction_id, "status": status,
    });
    if let (Some(obj), Some(summary)) = (patch.as_object_mut(), summary) {
        obj.insert("summary".to_owned(), json!([text_block(summary)]));
    }
    update(agent_session, patch)
}

pub(super) fn agent_started(agent: &str, agent_session_id: Option<&str>) -> Option<Frame> {
    let mut params = Map::new();
    params.insert("agent".to_owned(), json!(agent));
    if let Some(id) = agent_session_id {
        params.insert("agent_session_id".to_owned(), json!(id));
    }
    notification("_shepherd/agent/started", Value::Object(params))
}

pub(super) fn agent_idle() -> Option<Frame> {
    notification("_shepherd/agent/idle", json!({}))
}

pub(super) fn agent_exited() -> Option<Frame> {
    notification("_shepherd/agent/exited", json!({}))
}

/// `_claude-code/<hook>` carrying the payload: as the params if it is an object, else under `payload`.
pub(super) fn vendor(hook: &str, payload: &Value) -> Option<Frame> {
    let params = match payload {
        Value::Object(_) => payload.clone(),
        other => json!({ "payload": other }),
    };
    notification(&format!("_claude-code/{hook}"), params)
}

fn update(agent_session: &str, update: Value) -> Option<Frame> {
    synthesized(json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": agent_session, "update": update },
    }))
}

fn notification(method: &str, params: Value) -> Option<Frame> {
    Frame::from_value(json!({ "jsonrpc": "2.0", "method": method, "params": params })).ok()
}

fn synthesized(raw: Value) -> Option<Frame> {
    Frame::synthesized(raw).ok()
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}
