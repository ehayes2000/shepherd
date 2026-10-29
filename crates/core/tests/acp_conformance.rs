//! Conformance against the official ACP schema types (`agent-client-protocol-schema =1.13.0`, protocol v1).
//!
//! Every sample must be accepted by the schema crate, parse into our typed subset (not `Unknown`), and round-trip
//! through our types to the identical JSON value. The schema crate re-serializes each sample identically too, so
//! both readers agree on what these frames mean.

use agent_client_protocol_schema::v1 as schema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use shepherd_core::acpp::acp::{AcpFrame, PromptOutcome, SessionUpdate, StopReason};
use shepherd_core::acpp::{Correlated, Dir, Frame, PendingRequests};

/// Asserts the schema type accepts `v` and re-serializes it unchanged.
fn schema_accepts<T: DeserializeOwned + Serialize>(v: &Value) {
    let typed: T = serde_json::from_value(v.clone())
        .unwrap_or_else(|e| panic!("schema 1.13.0 rejects {v}: {e}"));
    assert_eq!(
        &serde_json::to_value(typed).unwrap_or_else(|e| panic!("{e}")),
        v,
        "schema re-serialization differs"
    );
}

fn ours(raw: &Value) -> Frame {
    let f = Frame::from_value(raw.clone()).unwrap_or_else(|e| panic!("we reject {raw}: {e}"));
    assert_eq!(
        &serde_json::to_value(&f).unwrap_or_else(|e| panic!("{e}")),
        raw,
        "our round-trip differs"
    );
    f
}

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn update(update: Value) -> Value {
    json!({ "sessionId": "sess_1", "update": update })
}

fn synthesized() -> Value {
    json!({ "shepherd": { "synthesized": true } })
}

#[test]
fn session_prompt_requests_conform() {
    for params in [
        json!({ "sessionId": "sess_1", "prompt": [{ "type": "text", "text": "fix the login bug" }] }),
        json!({ "sessionId": "sess_1", "_meta": synthesized(),
                "prompt": [{ "type": "text", "text": "hi" },
                           { "type": "resource_link", "uri": "file:///a.rs", "name": "a.rs" }] }),
    ] {
        schema_accepts::<schema::PromptRequest>(&params);
        let f = ours(&request(json!(1), "session/prompt", params));
        assert!(matches!(&f, Frame::Acp(m) if matches!(m.body(), AcpFrame::Prompt(_))));
    }
}

#[test]
fn session_update_notifications_conform() {
    let updates = [
        (
            "user_message_chunk",
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "hi" } }),
        ),
        (
            "tool_call",
            json!({ "sessionUpdate": "tool_call", "toolCallId": "toolu_1", "title": "Run ls", "name": "Bash",
                    "kind": "execute", "status": "in_progress", "rawInput": { "command": "ls" },
                    "_meta": synthesized() }),
        ),
        (
            "tool_call",
            json!({ "sessionUpdate": "tool_call", "toolCallId": "toolu_2", "title": "Think" }),
        ),
        (
            "tool_call_update",
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "toolu_1", "status": "completed" }),
        ),
        (
            "tool_call_update",
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "toolu_1", "status": "failed",
                    "title": "Run ls -la", "kind": "read" }),
        ),
        (
            "compaction_update",
            json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress" }),
        ),
        (
            "compaction_update",
            json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed",
                    "summary": [{ "type": "text", "text": "did things" }] }),
        ),
    ];
    for (kind, u) in updates {
        let params = update(u);
        schema_accepts::<schema::SessionNotification>(&params);
        let f = ours(&notification("session/update", params));
        let Frame::Acp(m) = &f else {
            panic!("{kind} parsed as {f:?}")
        };
        let AcpFrame::Update(n) = m.body() else {
            panic!("{kind} parsed as {m:?}")
        };
        assert!(
            !matches!(n.update, SessionUpdate::Other { .. }),
            "{kind} was not typed"
        );
    }
}

#[test]
fn unmodelled_session_updates_conform_as_other() {
    let params = update(json!({ "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "hello" } }));
    schema_accepts::<schema::SessionNotification>(&params);
    let f = ours(&notification("session/update", params));
    assert!(
        matches!(&f, Frame::Acp(m) if matches!(m.body(), AcpFrame::Update(n) if matches!(n.update, SessionUpdate::Other { .. })))
    );
}

#[test]
fn request_permission_conforms() {
    let params = json!({
        "sessionId": "sess_1",
        "toolCall": { "toolCallId": "toolu_1", "title": "rm -rf build" },
        "options": [{ "optionId": "allow", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "deny", "name": "Reject", "kind": "reject_once" }],
        "_meta": synthesized()
    });
    schema_accepts::<schema::RequestPermissionRequest>(&params);
    let f = ours(&request(
        json!("perm-1"),
        "session/request_permission",
        params,
    ));
    assert!(matches!(&f, Frame::Acp(m) if matches!(m.body(), AcpFrame::RequestPermission(_))));
}

#[test]
fn session_cancel_conforms() {
    let params = json!({ "sessionId": "sess_1" });
    schema_accepts::<schema::CancelNotification>(&params);
    let f = ours(&notification("session/cancel", params));
    assert!(matches!(&f, Frame::Acp(m) if matches!(m.body(), AcpFrame::Cancel(_))));
}

#[test]
fn prompt_responses_conform() {
    for (wire, ours_reason) in [
        ("end_turn", StopReason::EndTurn),
        ("max_tokens", StopReason::MaxTokens),
        ("max_turn_requests", StopReason::MaxTurnRequests),
        ("refusal", StopReason::Refusal),
        ("cancelled", StopReason::Cancelled),
    ] {
        let result = json!({ "stopReason": wire });
        schema_accepts::<schema::PromptResponse>(&result);

        let mut pending = PendingRequests::default();
        let req = ours(&request(
            json!(5),
            "session/prompt",
            json!({ "sessionId": "s", "prompt": [] }),
        ));
        pending.record(&Dir::ToAgent, &req);
        let Frame::Response(r) = ours(&json!({ "jsonrpc": "2.0", "id": 5, "result": result }))
        else {
            panic!("expected a response")
        };
        let Correlated::Prompt(PromptOutcome::Completed(p)) =
            pending.correlate(&Dir::FromAgent, &r)
        else {
            panic!("{wire} did not complete the prompt")
        };
        assert_eq!(p.stop_reason, ours_reason);
    }
}
