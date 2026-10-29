//! Typed frame builders: typed value → `to_frame()` → parse → the same typed value, with stable JSON.

use proptest::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use shepherd_core::acpp::acp::{
    AcpFrame, CancelNotification, CompactionStatus, CompactionUpdate, ContentBlock, ContentChunk,
    PromptOutcome, PromptRequest, PromptResponse, RequestPermissionRequest, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCall, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use shepherd_core::acpp::{
    AgentExited, AgentIdle, AgentStarted, Correlated, Dir, Frame, PendingRequests, RequestId,
    SessionClosed, SessionCreated, SessionRenamed, ShepherdFrame, WorkspaceSelected,
    WorktreeCreated, WorktreeHook,
};
use shepherd_core::ids::MachineId;

fn text(s: &str) -> String {
    serde_json::to_string(&json!(s)).unwrap_or_else(|e| panic!("{e}"))
}

fn bytes(f: &Frame) -> String {
    serde_json::to_string(f).unwrap_or_else(|e| panic!("{e}"))
}

fn acp(f: &Frame) -> &AcpFrame {
    match f {
        Frame::Acp(m) => m.body(),
        other => panic!("expected an ACP frame, got {other:?}"),
    }
}

fn shepherd(f: &Frame) -> &ShepherdFrame {
    match f {
        Frame::Shepherd(m) => m.body(),
        other => panic!("expected a _shepherd frame, got {other:?}"),
    }
}

fn message_id(f: &Frame) -> Option<&RequestId> {
    match f {
        Frame::Acp(m) => m.id(),
        Frame::Shepherd(m) => m.id(),
        other => panic!("expected a typed message, got {other:?}"),
    }
}

/// Reads a built `session/prompt` response back the way folds do: correlated with its request.
fn correlated_prompt(id: RequestId, response: &Frame) -> PromptOutcome {
    let request = PromptRequest {
        session_id: "a1".to_owned(),
        prompt: Vec::new(),
        meta: None,
    }
    .to_frame(id);
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &request);
    let Frame::Response(r) = response else {
        panic!("expected a response, got {response:?}");
    };
    match pending.correlate(&Dir::FromAgent, r) {
        Correlated::Prompt(outcome) => outcome,
        other => panic!("expected a prompt outcome, got {other:?}"),
    }
}

// ---------- examples ----------

#[test]
fn shepherd_frame_builds_a_notification_with_its_method_and_params() {
    let frame = ShepherdFrame::SessionRenamed(SessionRenamed {
        name: "api".to_owned(),
        auto: true,
    })
    .to_frame();
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","method":"_shepherd/session/renamed","params":{"name":"api","auto":true}}"#
    );
    assert_eq!(message_id(&frame), None);
}

#[test]
fn prompt_request_builds_a_request_with_the_given_id() {
    let frame = PromptRequest {
        session_id: "a1".to_owned(),
        prompt: vec![ContentBlock::text("hi")],
        meta: Some(json!({ "shepherd": { "continuation": true } })),
    }
    .to_frame(RequestId::from("p1"));
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","id":"p1","method":"session/prompt","params":{"sessionId":"a1","prompt":[{"type":"text","text":"hi"}],"_meta":{"shepherd":{"continuation":true}}}}"#
    );
    assert_eq!(message_id(&frame), Some(&RequestId::from("p1")));
}

#[test]
fn prompt_response_builds_a_result_with_stop_reason() {
    let frame = PromptResponse {
        stop_reason: StopReason::EndTurn,
        meta: None,
    }
    .to_frame(RequestId::from(7));
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}"#
    );
}

#[test]
fn tool_call_update_writes_only_the_fields_it_patches() {
    let frame = SessionNotification {
        session_id: "a1".to_owned(),
        update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: "t1".to_owned(),
            title: None,
            kind: None,
            status: Some(ToolCallStatus::Failed),
            raw_output: Some(json!("boom")),
            meta: None,
        }),
        meta: None,
    }
    .to_frame();
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"a1","update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed","rawOutput":"boom"}}}"#
    );
}

#[test]
fn tool_call_writes_its_tag_first_then_fields_in_order() {
    let frame = SessionNotification {
        session_id: "a1".to_owned(),
        update: SessionUpdate::ToolCall(ToolCall {
            tool_call_id: "t1".to_owned(),
            title: "Bash: ls".to_owned(),
            name: None,
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            raw_input: Some(json!({ "command": "ls" })),
            meta: None,
        }),
        meta: None,
    }
    .to_frame();
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"a1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Bash: ls","kind":"execute","status":"in_progress","rawInput":{"command":"ls"}}}}"#
    );
}

#[test]
fn compaction_update_writes_a_cleared_summary_as_null() {
    let frame = SessionNotification {
        session_id: "a1".to_owned(),
        update: SessionUpdate::CompactionUpdate(CompactionUpdate {
            compaction_id: "c1".to_owned(),
            status: CompactionStatus::Failed,
            summary: Some(Value::Null),
            error: None,
        }),
        meta: None,
    }
    .to_frame();
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"a1","update":{"sessionUpdate":"compaction_update","compactionId":"c1","status":"failed","summary":null}}}"#
    );
}

#[test]
fn request_permission_builds_a_request_with_tool_call_and_options() {
    let frame = RequestPermissionRequest {
        session_id: "a1".to_owned(),
        tool_call: ToolCallUpdate {
            tool_call_id: "t1".to_owned(),
            title: Some("Bash: rm".to_owned()),
            kind: None,
            status: None,
            raw_output: None,
            meta: None,
        },
        options: json!([]),
        meta: None,
    }
    .to_frame(RequestId::from("perm1"));
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","id":"perm1","method":"session/request_permission","params":{"sessionId":"a1","toolCall":{"toolCallId":"t1","title":"Bash: rm"},"options":[]}}"#
    );
}

#[test]
fn synthesized_marking_composes_with_a_built_request() {
    let frame = PromptRequest {
        session_id: "a1".to_owned(),
        prompt: Vec::new(),
        meta: Some(json!({ "shepherd": { "continuation": true } })),
    }
    .to_frame(RequestId::from("p1"))
    .into_synthesized()
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(frame.is_synthesized());
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","id":"p1","method":"session/prompt","params":{"sessionId":"a1","prompt":[],"_meta":{"shepherd":{"continuation":true,"synthesized":true}}}}"#
    );
}

#[test]
fn synthesized_marking_composes_with_a_built_response() {
    let frame = PromptResponse {
        stop_reason: StopReason::EndTurn,
        meta: None,
    }
    .to_frame(RequestId::from("p1"))
    .into_synthesized()
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(frame.is_synthesized());
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","id":"p1","result":{"stopReason":"end_turn","_meta":{"shepherd":{"synthesized":true}}}}"#
    );
}

#[test]
fn synthesized_marking_fails_on_a_non_object_meta() {
    let built = CancelNotification {
        session_id: "a1".to_owned(),
        meta: Some(json!(5)),
    }
    .to_frame();
    assert!(built.into_synthesized().is_err());
}

#[test]
fn notification_builder_takes_any_method() {
    let frame = Frame::notification("_claude-code/Stop", json!({ "x": 1 }));
    assert_eq!(
        bytes(&frame),
        r#"{"jsonrpc":"2.0","method":"_claude-code/Stop","params":{"x":1}}"#
    );
    assert!(matches!(frame, Frame::Unknown(_)));
}

#[test]
fn text_content_block_escapes_its_text() {
    let block = ContentBlock::text("a \"quote\"\n");
    let frame = PromptRequest {
        session_id: "a1".to_owned(),
        prompt: vec![block],
        meta: None,
    }
    .to_frame(RequestId::from(1));
    assert!(bytes(&frame).contains(&text("a \"quote\"\n")));
}

// ---------- properties ----------

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            prop::collection::btree_map(".{0,8}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// JSON other than `null`: an optional raw field holding `null` reads back as absent.
fn arb_present_json() -> impl Strategy<Value = Value> {
    arb_json().prop_filter("not null", |v| !v.is_null())
}

fn arb_object() -> impl Strategy<Value = Value> {
    prop::collection::btree_map(".{0,8}", arb_json(), 0..4)
        .prop_map(|m| Value::Object(m.into_iter().collect()))
}

fn arb_meta() -> impl Strategy<Value = Option<Value>> {
    proptest::option::of(arb_object())
}

fn arb_string() -> impl Strategy<Value = String> {
    ".{0,12}"
}

fn arb_open<T: From<String> + std::fmt::Debug>(
    known: &'static [&'static str],
) -> impl Strategy<Value = T> {
    prop_oneof![
        prop::sample::select(known).prop_map(str::to_owned),
        "[a-z_]{0,10}",
    ]
    .prop_map(T::from)
}

fn arb_kind() -> impl Strategy<Value = ToolKind> {
    arb_open(&[
        "read",
        "edit",
        "delete",
        "move",
        "search",
        "execute",
        "think",
        "fetch",
        "switch_mode",
        "other",
    ])
}

fn arb_status() -> impl Strategy<Value = ToolCallStatus> {
    arb_open(&["pending", "in_progress", "completed", "failed"])
}

fn arb_content() -> impl Strategy<Value = ContentBlock> {
    prop_oneof![
        (arb_string(), arb_meta())
            .prop_map(|(text, meta)| ContentBlock::Text(TextContent { text, meta })),
        (
            prop::sample::select(&["image", "audio", "resource_link"][..]),
            arb_object()
        )
            .prop_map(|(kind, extra)| {
                let mut raw = Map::new();
                raw.insert("type".to_owned(), json!(kind));
                if let Value::Object(extra) = extra {
                    raw.extend(extra.into_iter().filter(|(k, _)| k != "type"));
                }
                ContentBlock::Other(Value::Object(raw))
            }),
    ]
}

fn arb_tool_call_update() -> impl Strategy<Value = ToolCallUpdate> {
    (
        arb_string(),
        proptest::option::of(arb_string()),
        proptest::option::of(arb_kind()),
        proptest::option::of(arb_status()),
        proptest::option::of(arb_present_json()),
        arb_meta(),
    )
        .prop_map(
            |(tool_call_id, title, kind, status, raw_output, meta)| ToolCallUpdate {
                tool_call_id,
                title,
                kind,
                status,
                raw_output,
                meta,
            },
        )
}

fn arb_update() -> impl Strategy<Value = SessionUpdate> {
    prop_oneof![
        (
            arb_content(),
            proptest::option::of(arb_string()),
            arb_meta()
        )
            .prop_map(|(content, message_id, meta)| {
                SessionUpdate::UserMessageChunk(ContentChunk {
                    content,
                    message_id,
                    meta,
                })
            }),
        (
            arb_string(),
            arb_string(),
            proptest::option::of(arb_string()),
            arb_kind(),
            arb_status(),
            proptest::option::of(arb_present_json()),
            arb_meta(),
        )
            .prop_map(
                |(tool_call_id, title, name, kind, status, raw_input, meta)| {
                    SessionUpdate::ToolCall(ToolCall {
                        tool_call_id,
                        title,
                        name,
                        kind,
                        status,
                        raw_input,
                        meta,
                    })
                }
            ),
        arb_tool_call_update().prop_map(SessionUpdate::ToolCallUpdate),
        (
            arb_string(),
            arb_open::<CompactionStatus>(&["in_progress", "completed", "failed", "cancelled"]),
            proptest::option::of(arb_json()),
            proptest::option::of(arb_json()),
        )
            .prop_map(|(compaction_id, status, summary, error)| {
                SessionUpdate::CompactionUpdate(CompactionUpdate {
                    compaction_id,
                    status,
                    summary,
                    error,
                })
            }),
        (
            prop::sample::select(&["agent_message_chunk", "plan", "x_future"][..]),
            arb_object()
        )
            .prop_map(|(kind, extra)| {
                let mut raw = Map::new();
                raw.insert("sessionUpdate".to_owned(), json!(kind));
                if let Value::Object(extra) = extra {
                    raw.extend(extra.into_iter().filter(|(k, _)| k != "sessionUpdate"));
                }
                SessionUpdate::Other {
                    kind: kind.to_owned(),
                    raw: Value::Object(raw),
                }
            }),
    ]
}

fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(arb_string())
}

fn arb_shepherd() -> impl Strategy<Value = ShepherdFrame> {
    prop_oneof![
        (
            arb_string(),
            arb_opt_string(),
            arb_opt_string(),
            arb_opt_string(),
            arb_opt_string(),
            arb_opt_string()
        )
            .prop_map(|(name, workspace, worktree, branch, agent, machine)| {
                ShepherdFrame::SessionCreated(SessionCreated {
                    name,
                    workspace,
                    worktree,
                    branch,
                    agent,
                    machine: machine.map(MachineId::new),
                })
            }),
        (arb_string(), any::<bool>())
            .prop_map(|(name, auto)| ShepherdFrame::SessionRenamed(SessionRenamed { name, auto })),
        any::<bool>().prop_map(
            |worktree_removed| ShepherdFrame::SessionClosed(SessionClosed { worktree_removed })
        ),
        (
            arb_string(),
            arb_opt_string(),
            proptest::option::of(any::<u32>()),
            proptest::option::of(arb_present_json())
        )
            .prop_map(|(agent, agent_session_id, pid, command)| {
                ShepherdFrame::AgentStarted(AgentStarted {
                    agent,
                    agent_session_id,
                    pid,
                    command,
                })
            }),
        Just(ShepherdFrame::AgentIdle(AgentIdle {})),
        (
            proptest::option::of(any::<i32>()),
            proptest::option::of(any::<i32>())
        )
            .prop_map(|(code, signal)| ShepherdFrame::AgentExited(AgentExited { code, signal })),
        (arb_string(), arb_string(), arb_string()).prop_map(|(path, branch, base)| {
            ShepherdFrame::WorktreeCreated(WorktreeCreated { path, branch, base })
        }),
        (
            arb_string(),
            proptest::option::of(any::<i32>()),
            arb_string()
        )
            .prop_map(|(hook, exit_code, output_tail)| {
                ShepherdFrame::WorktreeHook(WorktreeHook {
                    hook,
                    exit_code,
                    output_tail,
                })
            }),
        arb_string().prop_map(|repo| ShepherdFrame::WorkspaceSelected(WorkspaceSelected { repo })),
    ]
}

fn arb_id() -> impl Strategy<Value = RequestId> {
    prop_oneof![
        Just(RequestId::Null),
        any::<i64>().prop_map(RequestId::from),
        arb_string().prop_map(RequestId::from),
    ]
}

fn arb_prompt() -> impl Strategy<Value = PromptRequest> {
    (
        arb_string(),
        prop::collection::vec(arb_content(), 0..3),
        arb_meta(),
    )
        .prop_map(|(session_id, prompt, meta)| PromptRequest {
            session_id,
            prompt,
            meta,
        })
}

fn arb_prompt_response() -> impl Strategy<Value = PromptResponse> {
    (
        arb_open::<StopReason>(&[
            "end_turn",
            "max_tokens",
            "max_turn_requests",
            "refusal",
            "cancelled",
        ]),
        arb_meta(),
    )
        .prop_map(|(stop_reason, meta)| PromptResponse { stop_reason, meta })
}

fn arb_permission() -> impl Strategy<Value = RequestPermissionRequest> {
    (
        arb_string(),
        arb_tool_call_update(),
        arb_present_json(),
        arb_meta(),
    )
        .prop_map(
            |(session_id, tool_call, options, meta)| RequestPermissionRequest {
                session_id,
                tool_call,
                options,
                meta,
            },
        )
}

fn arb_notification() -> impl Strategy<Value = SessionNotification> {
    (arb_string(), arb_update(), arb_meta()).prop_map(|(session_id, update, meta)| {
        SessionNotification {
            session_id,
            update,
            meta,
        }
    })
}

proptest! {
    #[test]
    fn shepherd_frames_round_trip(value in arb_shepherd()) {
        let frame = value.to_frame();
        prop_assert_eq!(frame.method(), Some(value.method()));
        prop_assert_eq!(message_id(&frame), None);
        let parsed = Frame::from_value(frame.raw().clone()).unwrap_or_else(|e| panic!("{e}"));
        prop_assert_eq!(shepherd(&parsed), &value);
        prop_assert_eq!(bytes(&shepherd(&parsed).to_frame()), bytes(&frame));
    }

    #[test]
    fn prompt_requests_round_trip(value in arb_prompt(), id in arb_id()) {
        let frame = value.to_frame(id.clone());
        let parsed = Frame::from_value(frame.raw().clone()).unwrap_or_else(|e| panic!("{e}"));
        prop_assert_eq!(message_id(&parsed), Some(&id));
        let AcpFrame::Prompt(back) = acp(&parsed) else { panic!("expected session/prompt") };
        prop_assert_eq!(back, &value);
        prop_assert_eq!(bytes(&back.to_frame(id)), bytes(&frame));
    }

    #[test]
    fn prompt_responses_round_trip(value in arb_prompt_response(), id in arb_id()) {
        let frame = value.to_frame(id.clone());
        prop_assert_eq!(frame.id(), Some(id.clone()));
        let PromptOutcome::Completed(back) = correlated_prompt(id.clone(), &frame) else {
            panic!("expected a completed prompt");
        };
        prop_assert_eq!(&back, &value);
        prop_assert_eq!(bytes(&back.to_frame(id)), bytes(&frame));
    }

    #[test]
    fn session_updates_round_trip(value in arb_notification()) {
        let frame = value.to_frame();
        prop_assert_eq!(frame.method(), Some("session/update"));
        let parsed = Frame::from_value(frame.raw().clone()).unwrap_or_else(|e| panic!("{e}"));
        let AcpFrame::Update(back) = acp(&parsed) else { panic!("expected session/update") };
        prop_assert_eq!(back, &value);
        prop_assert_eq!(bytes(&back.to_frame()), bytes(&frame));
    }

    #[test]
    fn permission_requests_round_trip(value in arb_permission(), id in arb_id()) {
        let frame = value.to_frame(id.clone());
        let parsed = Frame::from_value(frame.raw().clone()).unwrap_or_else(|e| panic!("{e}"));
        prop_assert_eq!(message_id(&parsed), Some(&id));
        let AcpFrame::RequestPermission(back) = acp(&parsed) else {
            panic!("expected session/request_permission")
        };
        prop_assert_eq!(back, &value);
        prop_assert_eq!(bytes(&back.to_frame(id)), bytes(&frame));
    }

    #[test]
    fn cancel_notifications_round_trip(session_id in arb_string(), meta in arb_meta()) {
        let value = CancelNotification { session_id, meta };
        let frame = value.to_frame();
        let parsed = Frame::from_value(frame.raw().clone()).unwrap_or_else(|e| panic!("{e}"));
        let AcpFrame::Cancel(back) = acp(&parsed) else { panic!("expected session/cancel") };
        prop_assert_eq!(back, &value);
    }

    #[test]
    fn built_frames_serialize_the_same_every_time(value in arb_notification()) {
        prop_assert_eq!(bytes(&value.to_frame()), bytes(&value.to_frame()));
    }

    #[test]
    fn synthesizing_a_built_frame_keeps_its_typed_body_and_meta(value in arb_notification()) {
        let marked = value.to_frame().into_synthesized().unwrap_or_else(|e| panic!("{e}"));
        prop_assert!(marked.is_synthesized());
        let AcpFrame::Update(back) = acp(&marked) else { panic!("expected session/update") };
        prop_assert_eq!(&back.update, &value.update);
        let meta = back.meta.clone().unwrap_or_else(|| panic!("no _meta"));
        for (k, v) in value.meta.iter().flat_map(|m| m.as_object()).flatten() {
            if k != "shepherd" {
                prop_assert_eq!(meta.get(k), Some(v));
            }
        }
    }
}

#[test]
fn prompt_response_meta_survives_correlation() {
    let value =
        PromptResponse::deserialize(json!({ "stopReason": "refusal", "_meta": { "k": 1 } }))
            .unwrap_or_else(|e| panic!("{e}"));
    let frame = value.to_frame(RequestId::from("p"));
    assert_eq!(
        correlated_prompt(RequestId::from("p"), &frame),
        PromptOutcome::Completed(value)
    );
}
