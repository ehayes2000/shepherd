//! ACP++ envelope and frame behavior, through the public `shepherd_core::acpp` API.

use serde_json::{Value, json};
use shepherd_core::acpp::acp::{
    AcpFrame, CompactionStatus, ContentBlock, PromptOutcome, SessionUpdate, StopReason,
    ToolCallStatus, ToolKind,
};
use shepherd_core::acpp::{
    AgentIdle, Correlated, Dir, ENVELOPE_VERSION, EntryId, Envelope, EnvelopeError, Frame,
    FrameDraft, FrameError, IdleInference, PendingRequests, RequestId, Seq, ShepherdFrame, Source,
    SourceKind, Stamp, Timestamp,
};
use shepherd_core::ids::{MachineId, SessionId};

fn frame(v: Value) -> Frame {
    Frame::from_value(v).unwrap_or_else(|e| panic!("expected a well-formed frame: {e}"))
}

fn reserialized(f: &Frame) -> Value {
    serde_json::to_value(f).unwrap_or_else(|e| panic!("{e}"))
}

fn acp(f: &Frame) -> &AcpFrame {
    match f {
        Frame::Acp(m) => m.body(),
        other => panic!("expected an ACP frame, got {other:?}"),
    }
}

fn update(f: &Frame) -> &SessionUpdate {
    match acp(f) {
        AcpFrame::Update(n) => &n.update,
        other => panic!("expected session/update, got {other:?}"),
    }
}

fn notify_update(update: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": "session/update",
            "params": { "sessionId": "a1", "update": update } })
}

// ---------- envelope ----------

fn full_envelope() -> Value {
    json!({
        "acpp": 1,
        "seq": 1042,
        "id": "01JBX7Q5W2M9K3ABCDEFGHJKMN",
        "ts": "2026-10-29T12:00:00.123Z",
        "machine": "m_7f3a9c",
        "session": "s_01JBX7P",
        "source": { "kind": "hook", "agent": "claude-code" },
        "dir": "from_agent",
        "frame": { "jsonrpc": "2.0", "method": "session/update",
                   "params": { "sessionId": "a1",
                               "update": { "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls" } } }
    })
}

#[test]
fn envelope_round_trips_every_field() {
    let raw = full_envelope();
    let env: Envelope = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(env.acpp(), 1);
    assert_eq!(env.seq(), Seq::new(1042));
    assert_eq!(env.id().as_str(), "01JBX7Q5W2M9K3ABCDEFGHJKMN");
    assert_eq!(env.ts().as_str(), "2026-10-29T12:00:00.123Z");
    assert_eq!(env.machine().as_str(), "m_7f3a9c");
    assert_eq!(env.session(), Some(&SessionId::new("s_01JBX7P")));
    assert_eq!(env.source().kind, SourceKind::Hook);
    assert_eq!(env.source().agent.as_deref(), Some("claude-code"));
    assert_eq!(env.dir(), &Dir::FromAgent);
    assert!(matches!(env.frame(), Frame::Acp(_)));
    assert_eq!(serde_json::to_value(&env).unwrap(), raw);
}

#[test]
fn host_level_envelope_keeps_null_session() {
    let mut raw = full_envelope();
    raw["session"] = Value::Null;
    raw["source"] = json!({ "kind": "shepherd" });
    raw["dir"] = json!("internal");
    let env: Envelope = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(env.session(), None);
    assert_eq!(env.source().kind, SourceKind::Shepherd);
    assert_eq!(serde_json::to_value(&env).unwrap(), raw);
}

#[test]
fn unknown_envelope_fields_survive_round_trip() {
    let mut raw = full_envelope();
    raw["relayed_from"] = json!({ "machine": "m_other", "seq": 7 });
    raw["source"]["version"] = json!("2.1.0");
    let env: Envelope = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(&env).unwrap(), raw);
}

#[test]
fn unknown_source_kind_and_dir_survive_round_trip() {
    let mut raw = full_envelope();
    raw["source"] = json!({ "kind": "relay" });
    raw["dir"] = json!("sideways");
    let env: Envelope = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(env.source().kind, SourceKind::Other("relay".into()));
    assert_eq!(env.dir(), &Dir::Other("sideways".into()));
    assert_eq!(serde_json::to_value(&env).unwrap(), raw);
}

#[test]
fn envelope_with_malformed_frame_is_rejected() {
    let mut raw = full_envelope();
    raw["frame"] = json!({ "method": "session/update" });
    assert!(serde_json::from_value::<Envelope>(raw).is_err());
}

#[test]
fn timestamp_accepts_rfc3339_and_keeps_the_string() {
    for s in [
        "2026-10-29T12:00:00.123Z",
        "2026-10-29T12:00:00Z",
        "2024-02-29T23:59:60.5+05:30",
        "2026-01-01t00:00:00z",
    ] {
        assert_eq!(Timestamp::parse(s).unwrap().as_str(), s);
    }
}

#[test]
fn timestamp_rejects_non_rfc3339() {
    for s in [
        "",
        "2026-10-29",
        "2026-13-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-10-29T24:00:00Z",
        "2026-10-29T12:00:00",
        "2026-10-29T12:00:00.Z",
    ] {
        assert!(Timestamp::parse(s).is_err(), "{s:?} should be rejected");
    }
}

/// jiff reads ISO 8601 / RFC 9557 forms that are not strict RFC 3339. They name a definite instant, so they're
/// accepted, and the original string is still kept.
#[test]
fn timestamp_accepts_iso8601_forms_beyond_rfc3339_and_keeps_the_string() {
    for s in [
        "2026-10-29 12:00:00Z",
        "2026-10-29T12:00:00+0530",
        // Basic format.
        "20261029T120000Z",
        "20261029T120000.123+0530",
        // Reduced precision.
        "2026-10-29T12:00Z",
        "2026-10-29T12Z",
        // Bracketed annotations (RFC 9557); the offset alone fixes the instant.
        "2026-10-29T12:00:00Z[America/New_York]",
        "2026-10-29T12:00:00+05:30[Asia/Kolkata]",
        "2026-10-29T12:00:00Z[u-ca=gregory]",
    ] {
        assert_eq!(Timestamp::parse(s).unwrap().as_str(), s);
    }
}

#[test]
fn timestamp_rejects_fractions_longer_than_nanoseconds() {
    assert!(Timestamp::parse("2026-10-29T12:00:00.123456789Z").is_ok());
    assert!(Timestamp::parse("2026-10-29T12:00:00.1234567890Z").is_err());
}

#[test]
fn timestamp_error_says_what_is_accepted() {
    let err = Timestamp::parse("yesterday").unwrap_err().to_string();
    assert!(err.contains("ISO 8601"), "{err}");
    assert!(!err.contains("RFC 3339"), "{err}");
}

#[test]
fn timestamps_compare_by_instant_not_by_string() {
    let ts = |s: &str| Timestamp::parse(s).unwrap();
    assert_eq!(
        ts("2026-10-29T12:30:00+01:00"),
        ts("2026-10-29T11:30:00.000Z")
    );
    assert!(ts("2026-10-29T12:00:00Z") < ts("2026-10-29T12:00:00.500Z"));
    assert!(ts("2026-10-29T12:30:00+01:00") < ts("2026-10-29T12:00:00Z"));
}

#[test]
fn envelope_with_invalid_timestamp_is_rejected() {
    let mut raw = full_envelope();
    raw["ts"] = json!("yesterday");
    assert!(serde_json::from_value::<Envelope>(raw).is_err());
}

#[test]
fn unsupported_envelope_version_is_a_typed_error() {
    let mut raw = full_envelope();
    raw["acpp"] = json!(2);
    assert_eq!(
        Envelope::from_value(raw.clone()),
        Err(EnvelopeError::UnsupportedVersion(2))
    );
    assert!(serde_json::from_value::<Envelope>(raw).is_err());
}

#[test]
fn malformed_envelope_is_a_typed_error() {
    let mut raw = full_envelope();
    raw.as_object_mut().unwrap().remove("seq");
    assert!(matches!(
        Envelope::from_value(raw),
        Err(EnvelopeError::Malformed(_))
    ));
}

fn idle_draft() -> FrameDraft {
    FrameDraft::new(
        frame(json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle" })),
        Source::new(SourceKind::Hook, Some("claude-code".into())),
        Dir::FromAgent,
        Some(SessionId::new("s_1")),
    )
}

#[test]
fn sealing_a_draft_assigns_the_append_time_fields() {
    let draft = idle_draft();
    let frame = draft.frame.clone();
    let stamp = Stamp {
        id: EntryId::new("01J"),
        ts: Timestamp::parse("2026-10-29T12:00:00.000Z").unwrap(),
        machine: MachineId::new("m_1"),
    };
    let env = draft.seal(Seq::new(7), stamp);
    assert_eq!(env.acpp(), ENVELOPE_VERSION);
    assert_eq!(env.seq(), Seq::new(7));
    assert_eq!(env.frame(), &frame);
    assert_eq!(
        serde_json::to_value(&env).unwrap(),
        json!({
            "acpp": 1, "seq": 7, "id": "01J", "ts": "2026-10-29T12:00:00.000Z", "machine": "m_1",
            "session": "s_1", "source": { "kind": "hook", "agent": "claude-code" }, "dir": "from_agent",
            "frame": { "jsonrpc": "2.0", "method": "_shepherd/agent/idle" }
        })
    );
}

#[test]
fn frame_draft_round_trips_through_json() {
    let draft = idle_draft();
    let wire = serde_json::to_value(&draft).unwrap();
    assert_eq!(
        wire,
        json!({ "frame": { "jsonrpc": "2.0", "method": "_shepherd/agent/idle" },
                "source": { "kind": "hook", "agent": "claude-code" },
                "dir": "from_agent", "session": "s_1" })
    );
    assert_eq!(serde_json::from_value::<FrameDraft>(wire).unwrap(), draft);
}

#[test]
fn host_level_frame_draft_round_trips_with_null_session() {
    let mut draft = idle_draft();
    draft.session = None;
    let wire = serde_json::to_value(&draft).unwrap();
    assert_eq!(wire["session"], Value::Null);
    assert_eq!(serde_json::from_value::<FrameDraft>(wire).unwrap(), draft);
}

// ---------- ACP frames ----------

#[test]
fn session_prompt_request_parses_as_typed_acp() {
    let f = frame(json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": { "sessionId": "a1", "prompt": [
            { "type": "text", "text": "fix the bug" },
            { "type": "image", "data": "AAAA", "mimeType": "image/png" }
        ] }
    }));
    assert_eq!(f.id(), Some(RequestId::from(3)));
    assert_eq!(f.method(), Some("session/prompt"));
    let AcpFrame::Prompt(p) = acp(&f) else {
        panic!("expected prompt")
    };
    assert_eq!(p.session_id, "a1");
    assert!(matches!(&p.prompt[0], ContentBlock::Text(t) if t.text == "fix the bug"));
    assert!(matches!(&p.prompt[1], ContentBlock::Other(v) if v["type"] == "image"));
}

#[test]
fn tool_call_update_parses_with_defaults() {
    let f = frame(notify_update(
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run ls" }),
    ));
    let SessionUpdate::ToolCall(tc) = update(&f) else {
        panic!("expected tool_call")
    };
    assert_eq!(tc.tool_call_id, "t1");
    assert_eq!(tc.title, "Run ls");
    assert_eq!(tc.kind, ToolKind::Other("other".into()));
    assert_eq!(tc.status, ToolCallStatus::Pending);
}

#[test]
fn tool_call_reads_known_kind_status_and_raw_input() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run ls", "name": "Bash",
        "kind": "execute", "status": "in_progress", "rawInput": { "command": "ls" }
    })));
    let SessionUpdate::ToolCall(tc) = update(&f) else {
        panic!("expected tool_call")
    };
    assert_eq!(tc.kind, ToolKind::Execute);
    assert_eq!(tc.status, ToolCallStatus::InProgress);
    assert_eq!(tc.name.as_deref(), Some("Bash"));
    assert_eq!(tc.raw_input, Some(json!({ "command": "ls" })));
}

#[test]
fn unknown_tool_kind_and_status_parse_as_other_and_round_trip() {
    let raw = notify_update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "x",
        "kind": "teleport", "status": "paused"
    }));
    let f = frame(raw.clone());
    let SessionUpdate::ToolCall(tc) = update(&f) else {
        panic!("expected tool_call")
    };
    assert_eq!(tc.kind, ToolKind::Other("teleport".into()));
    assert_eq!(tc.status, ToolCallStatus::Other("paused".into()));
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn tool_call_update_parses_optional_fields() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"
    })));
    let SessionUpdate::ToolCallUpdate(u) = update(&f) else {
        panic!("expected tool_call_update")
    };
    assert_eq!(u.tool_call_id, "t1");
    assert_eq!(u.status, Some(ToolCallStatus::Completed));
    assert_eq!(u.title, None);
    assert_eq!(u.kind, None);
}

#[test]
fn user_message_chunk_parses_content() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "user_message_chunk", "messageId": "m1",
        "content": { "type": "text", "text": "hello" }
    })));
    let SessionUpdate::UserMessageChunk(c) = update(&f) else {
        panic!("expected user_message_chunk")
    };
    assert_eq!(c.message_id.as_deref(), Some("m1"));
    assert!(matches!(&c.content, ContentBlock::Text(t) if t.text == "hello"));
}

#[test]
fn compaction_update_parses_status() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress"
    })));
    let SessionUpdate::CompactionUpdate(c) = update(&f) else {
        panic!("expected compaction_update")
    };
    assert_eq!(c.compaction_id, "c1");
    assert_eq!(c.status, CompactionStatus::InProgress);
}

#[test]
fn compaction_update_distinguishes_cleared_from_unchanged_fields() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed", "error": null
    })));
    let SessionUpdate::CompactionUpdate(c) = update(&f) else {
        panic!("expected compaction_update")
    };
    assert_eq!(c.error, Some(Value::Null));
    assert_eq!(c.summary, None);
}

#[test]
fn unknown_session_update_parses_as_other_and_round_trips() {
    let raw = notify_update(json!({ "sessionUpdate": "future_thing", "payload": [1, 2] }));
    let f = frame(raw.clone());
    let SessionUpdate::Other { kind, raw: inner } = update(&f) else {
        panic!("expected Other")
    };
    assert_eq!(kind, "future_thing");
    assert_eq!(inner["payload"], json!([1, 2]));
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn unmodelled_acp_session_update_parses_as_other() {
    let f = frame(notify_update(json!({
        "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" }
    })));
    assert!(
        matches!(update(&f), SessionUpdate::Other { kind, .. } if kind == "agent_message_chunk")
    );
}

#[test]
fn request_permission_parses_tool_call_title() {
    let f = frame(json!({
        "jsonrpc": "2.0", "id": "p1", "method": "session/request_permission",
        "params": { "sessionId": "a1",
                    "toolCall": { "toolCallId": "t9", "title": "rm -rf build" },
                    "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }] }
    }));
    let AcpFrame::RequestPermission(r) = acp(&f) else {
        panic!("expected request_permission")
    };
    assert_eq!(r.tool_call.tool_call_id, "t9");
    assert_eq!(r.tool_call.title.as_deref(), Some("rm -rf build"));
    assert_eq!(f.id(), Some(RequestId::from("p1")));
}

#[test]
fn session_cancel_parses() {
    let f = frame(
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "a1" } }),
    );
    assert!(matches!(acp(&f), AcpFrame::Cancel(c) if c.session_id == "a1"));
}

#[test]
fn unknown_meta_keys_are_preserved() {
    let raw = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
        "params": { "sessionId": "a1", "prompt": [],
                    "_meta": { "zed.dev": { "trace": "abc" }, "shepherd": { "future": 1 } } }
    });
    let f = frame(raw.clone());
    let AcpFrame::Prompt(p) = acp(&f) else {
        panic!("expected prompt")
    };
    assert_eq!(p.meta.as_ref().unwrap()["zed.dev"]["trace"], "abc");
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn unknown_fields_in_typed_frames_are_preserved() {
    let raw = notify_update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "x", "futureField": true
    }));
    let mut raw = raw;
    raw["params"]["futureTop"] = json!("kept");
    let f = frame(raw.clone());
    assert!(matches!(update(&f), SessionUpdate::ToolCall(_)));
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn unknown_method_parses_as_unknown_and_round_trips() {
    let raw = json!({ "jsonrpc": "2.0", "method": "_claude-code/compact", "params": { "trigger": "auto" } });
    let f = frame(raw.clone());
    assert_eq!(f, Frame::Unknown(raw.clone()));
    assert_eq!(f.method(), Some("_claude-code/compact"));
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn known_method_with_unexpected_shape_parses_as_unknown() {
    for raw in [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": { "prompt": "oops" } }),
        json!({ "jsonrpc": "2.0", "method": "session/prompt", "params": { "sessionId": "a", "prompt": [] } }),
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": [1, 2] }),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/renamed", "params": { "name": 5 } }),
    ] {
        let f = frame(raw.clone());
        assert_eq!(f, Frame::Unknown(raw.clone()));
        assert_eq!(reserialized(&f), raw);
    }
}

#[test]
fn frames_parse_from_json_text() {
    let f: Frame = serde_json::from_str(
        r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"a"}}"#,
    )
    .unwrap();
    assert!(matches!(f, Frame::Acp(_)));
}

// ---------- shepherd frames ----------

#[test]
fn shepherd_extension_table_parses_into_typed_frames() {
    let cases = [
        json!({ "method": "_shepherd/session/created",
                "params": { "name": "fix-login", "workspace": "~/code/shepherd", "worktree": "/w/fix",
                            "branch": "fix-login", "agent": "claude-code", "machine": "m_1" } }),
        json!({ "method": "_shepherd/session/renamed", "params": { "name": "n", "auto": true } }),
        json!({ "method": "_shepherd/session/closed", "params": { "worktree_removed": false } }),
        json!({ "method": "_shepherd/agent/started",
                "params": { "agent": "codex", "agent_session_id": "x", "pid": 42, "command": "codex" } }),
        json!({ "method": "_shepherd/agent/idle", "params": {} }),
        json!({ "method": "_shepherd/agent/exited", "params": { "code": 0 } }),
        json!({ "method": "_shepherd/worktree/created",
                "params": { "path": "/w/fix", "branch": "fix", "base": "origin/main" } }),
        json!({ "method": "_shepherd/worktree/hook",
                "params": { "hook": "post-create", "exit_code": 1, "output_tail": "err" } }),
        json!({ "method": "_shepherd/workspace/selected", "params": { "repo": "~/code/shepherd" } }),
    ];
    for mut raw in cases {
        raw["jsonrpc"] = json!("2.0");
        let f = frame(raw.clone());
        let Frame::Shepherd(m) = &f else {
            panic!(
                "{} should parse as a shepherd frame, got {f:?}",
                raw["method"]
            )
        };
        assert_eq!(m.body().method(), raw["method"]);
        assert_eq!(reserialized(&f), raw);
    }
}

#[test]
fn shepherd_frames_expose_typed_params() {
    let f = frame(
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/renamed",
                          "params": { "name": "auth-fix", "auto": true } }),
    );
    let Frame::Shepherd(m) = &f else { panic!() };
    let ShepherdFrame::SessionRenamed(r) = m.body() else {
        panic!("expected renamed")
    };
    assert_eq!(r.name, "auth-fix");
    assert!(r.auto);
}

#[test]
fn shepherd_frame_without_params_parses() {
    let f = frame(json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle" }));
    assert!(matches!(&f, Frame::Shepherd(m) if matches!(m.body(), ShepherdFrame::AgentIdle(_))));
}

fn idle_inference(params: Value) -> Option<IdleInference> {
    let f = frame(json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": params }));
    let Frame::Shepherd(m) = &f else {
        panic!("expected a shepherd frame, got {f:?}")
    };
    let ShepherdFrame::AgentIdle(idle) = m.body() else {
        panic!("expected agent/idle")
    };
    idle.inferred.clone()
}

#[test]
fn an_idle_says_how_it_was_inferred() {
    assert_eq!(idle_inference(json!({})), None);
    assert_eq!(
        idle_inference(json!({ "inferred": "quiet" })),
        Some(IdleInference::Quiet)
    );
    assert_eq!(
        idle_inference(json!({ "inferred": "quiet-after-permission" })),
        Some(IdleInference::QuietAfterPermission)
    );
}

#[test]
fn an_idle_inferred_some_new_way_is_still_an_idle() {
    assert_eq!(
        idle_inference(json!({ "inferred": "telepathy" })),
        Some(IdleInference::Other("telepathy".to_owned()))
    );
}

#[test]
fn an_inferred_idle_serializes_its_inference_and_a_plain_one_has_none() {
    let inferred = ShepherdFrame::AgentIdle(AgentIdle {
        inferred: Some(IdleInference::QuietAfterPermission),
    });
    assert_eq!(
        reserialized(&inferred.to_frame())["params"],
        json!({ "inferred": "quiet-after-permission" })
    );
    let plain = ShepherdFrame::AgentIdle(AgentIdle::default());
    assert_eq!(reserialized(&plain.to_frame())["params"], json!({}));
}

#[test]
fn unknown_shepherd_method_parses_as_unknown() {
    let raw =
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/pr", "params": { "url": "u" } });
    assert_eq!(frame(raw.clone()), Frame::Unknown(raw));
}

// ---------- responses ----------

#[test]
fn responses_parse_generically_with_any_id() {
    for id in [json!(1), json!("abc"), Value::Null] {
        let raw = json!({ "jsonrpc": "2.0", "id": id, "result": { "anything": true } });
        let f = frame(raw.clone());
        let Frame::Response(r) = &f else {
            panic!("expected response")
        };
        assert_eq!(serde_json::to_value(r.id()).unwrap(), id);
        assert_eq!(r.result(), Some(&json!({ "anything": true })));
        assert_eq!(r.error(), None);
        assert_eq!(reserialized(&f), raw);
    }
}

#[test]
fn error_responses_parse_generically() {
    let raw = json!({ "jsonrpc": "2.0", "id": 4, "error": { "code": -32603, "message": "boom" } });
    let f = frame(raw.clone());
    let Frame::Response(r) = &f else {
        panic!("expected response")
    };
    assert_eq!(r.result(), None);
    assert_eq!(r.error().unwrap()["message"], "boom");
    assert_eq!(reserialized(&f), raw);
}

#[test]
fn unknown_stop_reason_survives_round_trip() {
    let raw = json!({ "jsonrpc": "2.0", "id": 1,
                      "result": { "stopReason": "rate_limited", "_meta": { "x.y": 1 } } });
    let f = frame(raw.clone());
    assert!(matches!(f, Frame::Response(_)));
    assert_eq!(reserialized(&f), raw);
}

// ---------- malformed JSON-RPC ----------

#[test]
fn missing_jsonrpc_is_a_typed_error() {
    let err = Frame::from_value(json!({ "method": "session/cancel", "params": {} })).unwrap_err();
    assert_eq!(err, FrameError::MissingJsonRpc);
}

#[test]
fn wrong_jsonrpc_version_is_a_typed_error() {
    let err = Frame::from_value(json!({ "jsonrpc": "1.0", "method": "x" })).unwrap_err();
    assert!(matches!(err, FrameError::UnsupportedVersion(_)));
}

#[test]
fn result_and_error_together_is_a_typed_error() {
    let err = Frame::from_value(json!({ "jsonrpc": "2.0", "id": 1, "result": {}, "error": {} }))
        .unwrap_err();
    assert_eq!(err, FrameError::ResultAndError);
}

#[test]
fn other_malformed_messages_are_typed_errors() {
    assert_eq!(
        Frame::from_value(json!([1])).unwrap_err(),
        FrameError::NotAnObject
    );
    assert_eq!(
        Frame::from_value(json!({ "jsonrpc": "2.0", "method": 5 })).unwrap_err(),
        FrameError::InvalidMethod
    );
    assert_eq!(
        Frame::from_value(json!({ "jsonrpc": "2.0", "id": {}, "method": "m" })).unwrap_err(),
        FrameError::InvalidId
    );
    assert_eq!(
        Frame::from_value(json!({ "jsonrpc": "2.0", "result": {} })).unwrap_err(),
        FrameError::MissingId
    );
    assert_eq!(
        Frame::from_value(json!({ "jsonrpc": "2.0", "id": 1 })).unwrap_err(),
        FrameError::NotAMessage
    );
}

#[test]
fn envelope_json_with_malformed_frame_reports_the_frame_error() {
    let err = serde_json::from_str::<Frame>(r#"{"id":1,"result":{}}"#).unwrap_err();
    assert!(err.to_string().contains("jsonrpc"), "{err}");
}

// ---------- synthesized ----------

#[test]
fn synthesized_frames_are_marked_in_params_meta() {
    let f = Frame::synthesized(json!({
        "jsonrpc": "2.0", "id": "hook-1", "method": "session/prompt",
        "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": "hi" }] }
    }))
    .unwrap();
    assert!(f.is_synthesized());
    assert_eq!(
        f.raw()["params"]["_meta"]["shepherd"]["synthesized"],
        json!(true)
    );
    assert!(matches!(acp(&f), AcpFrame::Prompt(_)));
}

#[test]
fn marking_synthesized_keeps_existing_meta() {
    let f = Frame::synthesized(json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": "a1", "_meta": { "claude-code": { "hook": "PreToolUse" },
                                                  "shepherd": { "other": 1 } },
                    "update": { "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x" } }
    }))
    .unwrap();
    let meta = &f.raw()["params"]["_meta"];
    assert_eq!(meta["claude-code"]["hook"], "PreToolUse");
    assert_eq!(meta["shepherd"], json!({ "other": 1, "synthesized": true }));
    assert!(f.is_synthesized());
}

#[test]
fn synthesized_responses_are_marked_in_result_meta() {
    let f = Frame::synthesized(
        json!({ "jsonrpc": "2.0", "id": "hook-1", "result": { "stopReason": "end_turn" } }),
    )
    .unwrap();
    assert!(f.is_synthesized());
    assert_eq!(
        f.raw()["result"]["_meta"]["shepherd"]["synthesized"],
        json!(true)
    );
}

#[test]
fn synthesizing_a_notification_without_params_adds_them() {
    let f =
        Frame::synthesized(json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle" })).unwrap();
    assert!(f.is_synthesized());
    assert!(matches!(f, Frame::Shepherd(_)));
}

#[test]
fn real_frames_are_not_synthesized() {
    let f = frame(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                          "params": { "sessionId": "a1", "prompt": [], "_meta": { "shepherd": {} } } }),
    );
    assert!(!f.is_synthesized());
    let f = frame(json!({ "jsonrpc": "2.0", "id": 1, "result": {} }));
    assert!(!f.is_synthesized());
}

#[test]
fn continuation_prompts_are_recognized_by_their_mark() {
    let marked = |meta: Value| {
        frame(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                      "params": { "sessionId": "a1", "prompt": [], "_meta": meta } }),
        )
    };
    assert!(marked(json!({ "shepherd": { "continuation": true } })).is_continuation());
    assert!(!marked(json!({ "shepherd": { "continuation": false } })).is_continuation());
    assert!(!marked(json!({ "shepherd": {} })).is_continuation());
    assert!(!marked(json!({ "continuation": true })).is_continuation());
    let unmarked = frame(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                                 "params": { "sessionId": "a1", "prompt": [] } }),
    );
    assert!(!unmarked.is_continuation());
}

// ---------- correlating responses ----------

fn prompt_request(id: Value) -> Frame {
    frame(
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                  "params": { "sessionId": "a1", "prompt": [] } }),
    )
}

fn response(v: Value) -> shepherd_core::acpp::Response {
    match frame(v) {
        Frame::Response(r) => r,
        other => panic!("expected response, got {other:?}"),
    }
}

#[test]
fn prompt_response_is_typed_by_correlating_with_its_request() {
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!(7)));
    let r = response(json!({ "jsonrpc": "2.0", "id": 7, "result": { "stopReason": "end_turn" } }));
    let Correlated::Prompt(PromptOutcome::Completed(p)) = pending.correlate(&Dir::FromAgent, &r)
    else {
        panic!("expected a completed prompt")
    };
    assert_eq!(p.stop_reason, StopReason::EndTurn);
}

#[test]
fn unknown_stop_reason_still_completes_the_prompt() {
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!("p")));
    let r = response(
        json!({ "jsonrpc": "2.0", "id": "p", "result": { "stopReason": "rate_limited" } }),
    );
    let Correlated::Prompt(PromptOutcome::Completed(p)) = pending.correlate(&Dir::FromAgent, &r)
    else {
        panic!("expected a completed prompt")
    };
    assert_eq!(p.stop_reason, StopReason::Other("rate_limited".into()));
}

#[test]
fn prompt_error_and_malformed_results_are_reported() {
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!(1)));
    pending.record(&Dir::ToAgent, &prompt_request(json!(2)));
    let err =
        response(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": 1, "message": "no" } }));
    assert!(matches!(
        pending.correlate(&Dir::FromAgent, &err),
        Correlated::Prompt(PromptOutcome::Error(e)) if e["message"] == "no"
    ));
    let bad = response(json!({ "jsonrpc": "2.0", "id": 2, "result": { "nope": 1 } }));
    assert!(matches!(
        pending.correlate(&Dir::FromAgent, &bad),
        Correlated::Prompt(PromptOutcome::Malformed(_))
    ));
}

#[test]
fn responses_to_other_methods_report_the_method() {
    let mut pending = PendingRequests::default();
    pending.record(
        &Dir::Internal,
        &frame(json!({ "jsonrpc": "2.0", "id": 9, "method": "_vendor/ask", "params": {} })),
    );
    let r = response(json!({ "jsonrpc": "2.0", "id": 9, "result": {} }));
    assert_eq!(
        pending.correlate(&Dir::Internal, &r),
        Correlated::Other {
            method: "_vendor/ask".into()
        }
    );
}

#[test]
fn a_response_resolves_its_request_once() {
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!(1)));
    let r = response(json!({ "jsonrpc": "2.0", "id": 1, "result": { "stopReason": "end_turn" } }));
    assert!(matches!(
        pending.correlate(&Dir::FromAgent, &r),
        Correlated::Prompt(_)
    ));
    assert_eq!(
        pending.correlate(&Dir::FromAgent, &r),
        Correlated::Unmatched
    );
}

#[test]
fn notifications_are_not_pending_requests() {
    let mut pending = PendingRequests::default();
    pending.record(
        &Dir::ToAgent,
        &frame(
            json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "a" } }),
        ),
    );
    assert!(pending.is_empty());
}

#[test]
fn request_ids_are_scoped_by_direction() {
    // ACP peers number their requests independently, so a client's prompt and an agent's permission request can
    // share an id. A response answers the request that travelled the other way.
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!(0)));
    pending.record(
        &Dir::FromAgent,
        &frame(json!({ "jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
                       "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t" }, "options": [] } })),
    );
    let permission_answer = response(json!({ "jsonrpc": "2.0", "id": 0,
                                             "result": { "outcome": { "outcome": "cancelled" } } }));
    assert_eq!(
        pending.correlate(&Dir::ToAgent, &permission_answer),
        Correlated::Other {
            method: "session/request_permission".into()
        }
    );
    let prompt_answer =
        response(json!({ "jsonrpc": "2.0", "id": 0, "result": { "stopReason": "end_turn" } }));
    assert!(matches!(
        pending.correlate(&Dir::FromAgent, &prompt_answer),
        Correlated::Prompt(PromptOutcome::Completed(_))
    ));
}

#[test]
fn forgotten_request_no_longer_correlates() {
    let mut pending = PendingRequests::default();
    pending.record(&Dir::ToAgent, &prompt_request(json!(0)));
    pending.forget(&Dir::ToAgent, &RequestId::from(0));
    assert!(pending.is_empty());
    let answer =
        response(json!({ "jsonrpc": "2.0", "id": 0, "result": { "stopReason": "end_turn" } }));
    assert_eq!(
        pending.correlate(&Dir::FromAgent, &answer),
        Correlated::Unmatched
    );
}
