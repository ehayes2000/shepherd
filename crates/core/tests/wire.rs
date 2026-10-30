//! Client↔host wire messages, through the public `shepherd_core::wire` API.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use shepherd_core::acpp::{
    Dir, EntryId, Frame, FrameDraft, RequestId, Seq, Source, SourceKind, Stamp, Timestamp,
};
use shepherd_core::config::{AgentKind, Config};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    BuildId, ClientMessage, ClientNotification, ClientRequest, DecodeError, Decoder, DigestHasher,
    ExeDigest, Hello, HelloOk, HookFrame, HostMessage, HostNotification, HostResponse, HostResult,
    InputKind, LogEntry, Method, Notify, PROTOCOL_VERSION, ProfileInfo, PtyOutput, Request,
    RequestError, SessionClose, SessionInput, SessionNew, SessionNewOk, SessionRename,
    SessionResize, SessionResume, Subscribe, SubscribeFrom, WireError, WorkspaceSelect, code,
    decode_client_message, decode_host_message, encode_client_notification,
    encode_host_notification, encode_request, encode_response, negotiate,
};

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn draft() -> FrameDraft {
    let frame = Frame::from_value(json!({
        "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": { "extra": [1, 2] }
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    FrameDraft::new(
        frame,
        Source::new(SourceKind::Hook, Some("claude-code".into())),
        Dir::FromAgent,
        Some(sid("s_1")),
    )
}

/// A Claude Code hook payload as `shepherd hook` reads it from stdin.
fn hook_frame() -> HookFrame {
    HookFrame {
        session: sid("s_1"),
        agent: AgentKind::ClaudeCode,
        payload: json!({ "hook_event_name": "Stop", "session_id": "abc", "extra": [1, 2] }),
    }
}

fn one_of_each_request() -> Vec<ClientRequest> {
    vec![
        ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_1"),
            cwd: None,
            build: None,
        }),
        ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_1"),
            cwd: Some("/src/shepherd/crates".into()),
            build: None,
        }),
        ClientRequest::SessionNew(SessionNew {
            profile: Some("claude".into()),
            cwd: Some("/src/shepherd".into()),
        }),
        ClientRequest::SessionNew(SessionNew::default()),
        ClientRequest::SessionRename(SessionRename {
            session: sid("s_1"),
            name: "fix-login".into(),
        }),
        ClientRequest::SessionClose(SessionClose {
            session: sid("s_1"),
            worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        }),
        ClientRequest::Subscribe(Subscribe {
            from: SubscribeFrom {
                host: Some(Seq::new(3)),
                sessions: BTreeMap::from([(sid("s_1"), Seq::new(7)), (sid("s_2"), Seq::new(1))]),
            },
        }),
        ClientRequest::Subscribe(Subscribe::default()),
        ClientRequest::HookFrame(Box::new(hook_frame())),
        ClientRequest::SessionResume(SessionResume {
            session: sid("s_1"),
        }),
        ClientRequest::WorkspaceSelect(WorkspaceSelect {
            path: "/src/shepherd".into(),
        }),
        ClientRequest::SessionClose(SessionClose {
            session: sid("s_1"),
            worktree: shepherd_core::wire::CloseWorktree::Remove {
                confirmed: shepherd_core::ports::WorktreeStatus {
                    dirty: true,
                    ahead: 1,
                    pushed: false,
                    merged_into_default: false,
                },
            },
        }),
        ClientRequest::WorktreeStatus(shepherd_core::wire::WorktreeCheck {
            session: sid("s_1"),
        }),
    ]
}

fn one_of_each_notification() -> Vec<ClientNotification> {
    vec![
        ClientNotification::SessionInput(SessionInput {
            session: sid("s_1"),
            data: vec![0x1b, b'[', b'A', 0xff, 0x00],
            kind: InputKind::Mouse,
        }),
        ClientNotification::SessionResize(SessionResize {
            session: sid("s_1"),
            cols: 120,
            rows: 40,
        }),
    ]
}

/// Strips the line ending and parses the line as JSON.
fn wire_json(line: &[u8]) -> Value {
    assert_eq!(
        line.last(),
        Some(&b'\n'),
        "encoded messages end in a newline"
    );
    assert_eq!(
        line.iter().filter(|&&b| b == b'\n').count(),
        1,
        "an encoded message is exactly one line"
    );
    serde_json::from_slice(line).unwrap_or_else(|e| panic!("{e}"))
}

fn request_line(method: &str, params: Value) -> Vec<u8> {
    json!({ "jsonrpc": "2.0", "id": 9, "method": method, "params": params })
        .to_string()
        .into_bytes()
}

fn notification_line(method: &str, params: Value) -> Vec<u8> {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
        .to_string()
        .into_bytes()
}

fn request(line: &[u8]) -> Request {
    match decode_client_message(line).unwrap_or_else(|e| panic!("{e}")) {
        ClientMessage::Request(r) => r,
        other => panic!("expected a request, got {other:?}"),
    }
}

fn client_notification(line: &[u8]) -> ClientNotification {
    match decode_client_message(line).unwrap_or_else(|e| panic!("{e}")) {
        ClientMessage::Notification(n) => n,
        other => panic!("expected a notification, got {other:?}"),
    }
}

fn rejection(line: &[u8]) -> RequestError {
    let Err(err) = decode_client_message(line) else {
        panic!("expected the message to be rejected");
    };
    err
}

fn rejected(line: &[u8]) -> (Option<RequestId>, WireError) {
    let err = rejection(line);
    (err.id, err.error)
}

/// The JSON the host would answer a rejected message with, or `None` if it stays silent.
fn answer(line: &[u8]) -> Option<Value> {
    rejection(line)
        .into_response()
        .map(|r| wire_json(&encode_response(&r)))
}

fn response(line: &[u8], method: Method) -> HostResponse {
    match decode_host_message(line).unwrap_or_else(|e| panic!("{e}")) {
        HostMessage::Response(r) => r.typed(method).unwrap_or_else(|e| panic!("{e}")),
        other => panic!("expected a response, got {other:?}"),
    }
}

fn notification(line: &[u8]) -> HostNotification {
    match decode_host_message(line).unwrap_or_else(|e| panic!("{e}")) {
        HostMessage::Notification(n) => n,
        other => panic!("expected a notification, got {other:?}"),
    }
}

// ---------- requests ----------

#[test]
fn every_client_request_round_trips_through_encode_and_decode() {
    for (i, body) in one_of_each_request().into_iter().enumerate() {
        let req = Request {
            id: RequestId::from(i as i64),
            body,
        };
        assert_eq!(request(&encode_request(&req)), req);
    }
}

#[test]
fn requests_are_jsonrpc_with_shepherd_method_names() {
    let expected = [
        "_shepherd/hello",
        "_shepherd/hello",
        "_shepherd/session/new",
        "_shepherd/session/new",
        "_shepherd/session/rename",
        "_shepherd/session/close",
        "_shepherd/subscribe",
        "_shepherd/subscribe",
        "_shepherd/hook/frame",
        "_shepherd/session/resume",
        "_shepherd/workspace/select",
        "_shepherd/session/close",
        "_shepherd/worktree/status",
    ];
    for (body, method) in one_of_each_request().into_iter().zip(expected) {
        assert_eq!(body.method().as_str(), method);
        let v = wire_json(&encode_request(&Request {
            id: "r1".into(),
            body,
        }));
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "r1");
        assert_eq!(v["method"], method);
    }
}

#[test]
fn hello_params_are_version_and_machine() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::Hello(Hello {
            version: 1,
            machine: MachineId::new("m_7f"),
            cwd: None,
            build: None,
        }),
    });
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "version": 1, "machine": "m_7f" })
    );
}

#[test]
fn hello_carries_the_clients_cwd_and_a_hello_without_one_still_parses() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::Hello(Hello {
            version: 1,
            machine: MachineId::new("m_7f"),
            cwd: Some("/src/app".into()),
            build: None,
        }),
    });
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "version": 1, "machine": "m_7f", "cwd": "/src/app" })
    );
    let older = request_line(
        "_shepherd/hello",
        json!({ "version": 1, "machine": "m_7f" }),
    );
    let ClientRequest::Hello(hello) = request(&older).body else {
        panic!("not a hello");
    };
    assert_eq!(hello.cwd, None);
}

#[test]
fn workspace_select_params_are_the_path() {
    let line = request_line("_shepherd/workspace/select", json!({ "path": "/src/app" }));
    assert_eq!(
        request(&line).body,
        ClientRequest::WorkspaceSelect(WorkspaceSelect {
            path: "/src/app".into()
        })
    );
}

#[test]
fn subscribe_from_maps_host_and_sessions_to_seqs() {
    let line = request_line(
        "_shepherd/subscribe",
        json!({ "from": { "host": 12, "sessions": { "s_1": 4 } } }),
    );
    assert_eq!(
        request(&line).body,
        ClientRequest::Subscribe(Subscribe {
            from: SubscribeFrom {
                host: Some(Seq::new(12)),
                sessions: BTreeMap::from([(sid("s_1"), Seq::new(4))]),
            }
        })
    );
}

#[test]
fn subscribe_from_nothing_omits_host_and_sessions() {
    let line = request_line("_shepherd/subscribe", json!({ "from": {} }));
    assert_eq!(
        request(&line).body,
        ClientRequest::Subscribe(Subscribe::default())
    );
    let encoded = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::Subscribe(Subscribe::default()),
    });
    assert_eq!(
        wire_json(&encoded)["params"],
        json!({ "from": { "sessions": {} } })
    );
}

#[test]
fn session_new_without_params_uses_defaults() {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "_shepherd/session/new" }).to_string();
    assert_eq!(
        request(line.as_bytes()).body,
        ClientRequest::SessionNew(SessionNew::default())
    );
}

#[test]
fn hook_frame_carries_the_session_agent_and_raw_payload_verbatim() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::HookFrame(Box::new(hook_frame())),
    });
    let v = wire_json(&line);
    assert_eq!(
        v["params"],
        json!({
            "session": "s_1",
            "agent": "claude-code",
            "payload": { "hook_event_name": "Stop", "session_id": "abc", "extra": [1, 2] },
        })
    );
}

#[test]
fn hook_frame_payload_may_be_any_json() {
    let line = request_line(
        "_shepherd/hook/frame",
        json!({ "session": "s_1", "agent": "claude-code", "payload": "not an object" }),
    );
    let ClientRequest::HookFrame(hook) = request(&line).body else {
        panic!("expected a hook frame");
    };
    assert_eq!(hook.payload, json!("not an object"));
}

#[test]
fn hook_frame_for_an_unknown_agent_kind_is_invalid_params() {
    let line = request_line(
        "_shepherd/hook/frame",
        json!({ "session": "s_1", "agent": "robot", "payload": {} }),
    );
    let err = decode_client_message(&line).expect_err("refused");
    assert!(matches!(err.error, WireError::InvalidParams(_)), "{err:?}");
}

#[test]
fn unknown_request_fields_are_ignored() {
    let line = request_line(
        "_shepherd/session/close",
        json!({ "session": "s_1", "force": true }),
    );
    assert_eq!(
        request(&line).body,
        ClientRequest::SessionClose(SessionClose {
            session: sid("s_1"),
            worktree: shepherd_core::wire::CloseWorktree::IfSafe,
        })
    );
}

#[test]
fn unknown_method_is_method_not_found_with_the_request_id() {
    let line = request_line("_shepherd/teleport", json!({}));
    assert_eq!(
        rejected(&line),
        (
            Some(9.into()),
            WireError::MethodNotFound("_shepherd/teleport".into())
        )
    );
    assert_eq!(answer(&line).map(|v| v["id"].clone()), Some(json!(9)));
}

#[test]
fn malformed_json_is_a_parse_error_answered_with_a_null_id() {
    let (id, error) = rejected(b"{\"jsonrpc\":");
    assert_eq!(id, None);
    assert!(matches!(error, WireError::Parse(_)), "{error:?}");

    let v = answer(b"{\"jsonrpc\":").unwrap_or_else(|| panic!("a parse error is answered"));
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["code"], code::PARSE_ERROR);
}

#[test]
fn malformed_message_without_an_id_is_still_answered_with_a_null_id() {
    let line = json!({ "jsonrpc": "2.0", "method": 1 }).to_string();
    let v = answer(line.as_bytes()).unwrap_or_else(|| panic!("an invalid request is answered"));
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["code"], code::INVALID_REQUEST);
}

#[test]
fn request_method_sent_as_a_notification_is_dropped_unanswered() {
    let line = notification_line("_shepherd/session/close", json!({ "session": "s_1" }));
    let (id, error) = rejected(&line);
    assert_eq!(id, None);
    assert!(matches!(error, WireError::InvalidRequest(_)), "{error:?}");
    assert_eq!(answer(&line), None);
}

#[test]
fn request_with_wrong_jsonrpc_version_is_invalid() {
    let line = json!({ "jsonrpc": "1.0", "id": 4, "method": "_shepherd/session/new" }).to_string();
    let (id, error) = rejected(line.as_bytes());
    assert_eq!(id, Some(4.into()));
    assert!(matches!(error, WireError::InvalidRequest(_)), "{error:?}");
}

#[test]
fn request_with_params_of_the_wrong_shape_is_invalid_params() {
    let line = request_line("_shepherd/session/rename", json!({ "session": "s_1" }));
    let (id, error) = rejected(&line);
    assert_eq!(id, Some(9.into()));
    assert!(matches!(error, WireError::InvalidParams(_)), "{error:?}");
}

// ---------- client notifications ----------

#[test]
fn every_client_notification_round_trips_through_encode_and_decode() {
    for note in one_of_each_notification() {
        assert_eq!(
            client_notification(&encode_client_notification(&note)),
            note
        );
    }
}

#[test]
fn input_and_resize_are_notifications_without_an_id() {
    let expected = ["_shepherd/session/input", "_shepherd/session/resize"];
    for (note, method) in one_of_each_notification().into_iter().zip(expected) {
        assert_eq!(note.method(), method);
        let v = wire_json(&encode_client_notification(&note));
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], method);
        assert!(v.get("id").is_none(), "notifications have no id");
    }
}

#[test]
fn input_bytes_are_base64_on_the_wire() {
    let line = encode_client_notification(&ClientNotification::SessionInput(SessionInput {
        session: sid("s_1"),
        data: b"foobar".to_vec(),
        kind: InputKind::Keys,
    }));
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "session": "s_1", "data": "Zm9vYmFy" })
    );
}

#[test]
fn input_says_what_kind_of_input_it_is_unless_it_is_keys() {
    let kinds = [
        (InputKind::Paste, json!("paste")),
        (InputKind::Focus, json!("focus")),
        (InputKind::Mouse, json!("mouse")),
        (InputKind::Answer, json!("answer")),
    ];
    for (kind, name) in kinds {
        let line = encode_client_notification(&ClientNotification::SessionInput(SessionInput {
            session: sid("s_1"),
            data: b"x".to_vec(),
            kind,
        }));
        assert_eq!(wire_json(&line)["params"]["kind"], name, "{kind:?}");
    }
}

#[test]
fn input_from_a_client_older_than_kinds_is_keys() {
    let line = notification_line(
        "_shepherd/session/input",
        json!({ "session": "s_1", "data": "eA==" }),
    );
    let ClientNotification::SessionInput(input) = client_notification(&line) else {
        panic!("expected input");
    };
    assert_eq!(input.kind, InputKind::Keys);
}

#[test]
fn input_of_a_kind_this_host_does_not_know_is_still_written() {
    // A newer client's kind must not drop its keystrokes.
    let line = notification_line(
        "_shepherd/session/input",
        json!({ "session": "s_1", "data": "eA==", "kind": "gesture" }),
    );
    let ClientNotification::SessionInput(input) = client_notification(&line) else {
        panic!("expected input");
    };
    assert_eq!(input.data, b"x");
    assert_eq!(input.kind, InputKind::Other);
}

#[test]
fn resize_params_are_cols_and_rows() {
    let line = notification_line(
        "_shepherd/session/resize",
        json!({ "session": "s_1", "cols": 80, "rows": 24 }),
    );
    assert_eq!(
        client_notification(&line),
        ClientNotification::SessionResize(SessionResize {
            session: sid("s_1"),
            cols: 80,
            rows: 24
        })
    );
}

#[test]
fn input_or_resize_sent_as_a_request_is_answered_invalid_request() {
    for (method, params) in [
        (
            "_shepherd/session/input",
            json!({ "session": "s_1", "data": "" }),
        ),
        (
            "_shepherd/session/resize",
            json!({ "session": "s_1", "cols": 80, "rows": 24 }),
        ),
    ] {
        let line = request_line(method, params);
        let (id, error) = rejected(&line);
        assert_eq!(id, Some(9.into()), "{method}");
        assert!(matches!(error, WireError::InvalidRequest(_)), "{error:?}");
        let v = answer(&line).unwrap_or_else(|| panic!("a request is always answered"));
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], code::INVALID_REQUEST);
    }
}

#[test]
fn notification_with_invalid_params_is_dropped_unanswered() {
    let bad = [
        notification_line(
            "_shepherd/session/input",
            json!({ "session": "s_1", "data": "Zm9v!" }),
        ),
        notification_line(
            "_shepherd/session/resize",
            json!({ "session": "s_1", "cols": -1 }),
        ),
    ];
    for line in bad {
        let (id, error) = rejected(&line);
        assert_eq!(id, None);
        assert!(matches!(error, WireError::InvalidParams(_)), "{error:?}");
        assert_eq!(answer(&line), None);
    }
}

#[test]
fn unknown_client_notification_is_dropped_unanswered() {
    let line = notification_line("_shepherd/teleport", json!({}));
    assert!(matches!(rejected(&line).1, WireError::MethodNotFound(_)));
    assert_eq!(answer(&line), None);
}

// ---------- hello ----------

#[test]
fn hello_with_matching_version_returns_host_version_and_machine() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("m_client"),
        cwd: None,
        build: None,
    };
    assert_eq!(
        negotiate(&hello, &MachineId::new("m_host")),
        Ok(HelloOk {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_host"),
            profiles: Vec::new(),
            default_profile: None,
            notify: Notify::default(),
            build: None,
            installed: None,
        })
    );
}

fn config(value: Value) -> Config {
    let raw = serde_json::from_value(value).unwrap_or_else(|e| panic!("bad RawConfig: {e}"));
    Config::resolve(Some(raw), None).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn hello_ok_lists_the_hosts_profiles_and_names_the_default() {
    let config = config(json!({
        "defaults": { "agent": "codex" },
        "agents": { "review": { "kind": "claude-code", "command": "claude" } }
    }));
    let ok = negotiate(&hello(), &MachineId::new("m_host"))
        .unwrap_or_else(|e| panic!("{e}"))
        .with_profiles(&config);
    assert_eq!(ok.default_profile.as_deref(), Some("codex"));
    assert_eq!(
        ok.profiles,
        [
            ProfileInfo {
                name: "claude".into(),
                agent: AgentKind::ClaudeCode
            },
            ProfileInfo {
                name: "codex".into(),
                agent: AgentKind::Codex
            },
            ProfileInfo {
                name: "review".into(),
                agent: AgentKind::ClaudeCode
            },
            ProfileInfo {
                name: "shell".into(),
                agent: AgentKind::Shell
            },
        ]
    );
}

#[test]
fn hello_ok_profiles_are_name_and_agent_on_the_wire() {
    let config = config(json!({}));
    let ok = negotiate(&hello(), &MachineId::new("m_host"))
        .unwrap_or_else(|e| panic!("{e}"))
        .with_profiles(&config);
    let line = encode_response(&HostResponse {
        id: 1.into(),
        result: Ok(HostResult::Hello(ok)),
    });
    let result = &wire_json(&line)["result"];
    assert_eq!(result["default_profile"], "claude");
    assert_eq!(
        result["profiles"][0],
        json!({ "name": "claude", "agent": "claude-code" })
    );
}

#[test]
fn a_hello_ok_from_an_older_host_has_no_profiles() {
    let line = json!({ "jsonrpc": "2.0", "id": 1,
                       "result": { "version": PROTOCOL_VERSION, "machine": "m_host" } })
    .to_string();
    let resp = response(line.as_bytes(), Method::Hello);
    assert_eq!(
        resp.result,
        Ok(HostResult::Hello(HelloOk {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_host"),
            profiles: Vec::new(),
            default_profile: None,
            notify: Notify::default(),
            build: None,
            installed: None,
        }))
    );
}

#[test]
fn hello_ok_carries_the_hosts_bell_setting() {
    let config = config(json!({ "notify": { "bell": true } }));
    let ok = negotiate(&hello(), &MachineId::new("m_host"))
        .unwrap_or_else(|e| panic!("{e}"))
        .with_notify(&config);
    assert_eq!(ok.notify, Notify { bell: true });
    let line = encode_response(&HostResponse {
        id: 1.into(),
        result: Ok(HostResult::Hello(ok)),
    });
    assert_eq!(
        wire_json(&line)["result"]["notify"],
        json!({ "bell": true })
    );
}

#[test]
fn hello_ok_with_the_bell_off_leaves_notify_off_the_wire() {
    let ok = negotiate(&hello(), &MachineId::new("m_host"))
        .unwrap_or_else(|e| panic!("{e}"))
        .with_notify(&config(json!({})));
    let line = encode_response(&HostResponse {
        id: 1.into(),
        result: Ok(HostResult::Hello(ok)),
    });
    assert_eq!(wire_json(&line)["result"].get("notify"), None);
}

fn hello() -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("m_client"),
        cwd: None,
        build: None,
    }
}

#[test]
fn hello_with_other_version_gets_a_typed_version_mismatch_response() {
    let hello = Hello {
        version: PROTOCOL_VERSION + 1,
        machine: MachineId::new("m_client"),
        cwd: None,
        build: None,
    };
    let result = negotiate(&hello, &MachineId::new("m_host")).map(HostResult::Hello);
    let mismatch = WireError::VersionMismatch {
        host: PROTOCOL_VERSION,
        client: PROTOCOL_VERSION + 1,
    };
    assert_eq!(result, Err(mismatch.clone()));

    let line = encode_response(&HostResponse {
        id: 1.into(),
        result,
    });
    let v = wire_json(&line);
    assert_eq!(v["error"]["code"], 1000);
    assert_eq!(
        v["error"]["data"],
        json!({ "host": PROTOCOL_VERSION, "client": PROTOCOL_VERSION + 1 })
    );
    assert_eq!(response(&line, Method::Hello).result, Err(mismatch));
}

// ---------- responses ----------

#[test]
fn successful_responses_round_trip_by_method() {
    let cases = [
        (
            Method::Hello,
            HostResult::Hello(HelloOk {
                version: 1,
                machine: MachineId::new("m_1"),
                profiles: vec![ProfileInfo {
                    name: "codex".into(),
                    agent: AgentKind::Codex,
                }],
                default_profile: Some("codex".into()),
                notify: Notify { bell: true },
                build: None,
                installed: None,
            }),
        ),
        (
            Method::SessionNew,
            HostResult::SessionNew(SessionNewOk {
                session: sid("s_9"),
            }),
        ),
        (Method::SessionClose, HostResult::Ack),
        (Method::HookFrame, HostResult::Ack),
        (Method::SessionResume, HostResult::Ack),
        (
            Method::WorktreeStatus,
            HostResult::WorktreeStatus(shepherd_core::wire::WorktreeStatusOk {
                status: Some(shepherd_core::ports::WorktreeStatus {
                    dirty: true,
                    ahead: 2,
                    pushed: false,
                    merged_into_default: false,
                }),
            }),
        ),
        (
            Method::WorktreeStatus,
            HostResult::WorktreeStatus(shepherd_core::wire::WorktreeStatusOk::default()),
        ),
    ];
    for (method, result) in cases {
        let resp = HostResponse {
            id: "r".into(),
            result: Ok(result),
        };
        assert_eq!(response(&encode_response(&resp), method), resp);
    }
}

#[test]
fn session_new_result_is_the_session_id() {
    let line = encode_response(&HostResponse {
        id: 2.into(),
        result: Ok(HostResult::SessionNew(SessionNewOk {
            session: sid("s_9"),
        })),
    });
    assert_eq!(
        wire_json(&line),
        json!({ "jsonrpc": "2.0", "id": 2, "result": { "session": "s_9" } })
    );
}

#[test]
fn a_close_says_what_to_do_with_the_worktree_and_what_removing_it_anyway_confirmed() {
    let params = |worktree| {
        wire_json(&encode_request(&Request {
            id: 1.into(),
            body: ClientRequest::SessionClose(SessionClose {
                session: sid("s_1"),
                worktree,
            }),
        }))["params"]
            .clone()
    };
    use shepherd_core::wire::CloseWorktree;
    assert_eq!(params(CloseWorktree::IfSafe), json!({ "session": "s_1" }));
    let confirmed = shepherd_core::ports::WorktreeStatus {
        dirty: true,
        ahead: 1,
        pushed: false,
        merged_into_default: false,
    };
    assert_eq!(
        params(CloseWorktree::Remove { confirmed }),
        json!({ "session": "s_1", "worktree": { "remove": { "confirmed": {
            "dirty": true, "ahead": 1, "pushed": false, "merged_into_default": false
        } } } })
    );
    assert_eq!(
        params(CloseWorktree::Keep),
        json!({ "session": "s_1", "worktree": "keep" })
    );
}

#[test]
fn ack_is_an_empty_object() {
    let line = encode_response(&HostResponse {
        id: 2.into(),
        result: Ok(HostResult::Ack),
    });
    assert_eq!(wire_json(&line)["result"], json!({}));
}

#[test]
fn every_error_round_trips_with_a_stable_code() {
    let cases = [
        (WireError::Parse("eof".into()), -32700),
        (WireError::InvalidRequest("no id".into()), -32600),
        (WireError::MethodNotFound("_x".into()), -32601),
        (WireError::InvalidParams("cols".into()), -32602),
        (WireError::Internal("oops".into()), -32603),
        (WireError::VersionMismatch { host: 1, client: 2 }, 1000),
        (
            WireError::UnknownSession {
                session: sid("s_404"),
            },
            1001,
        ),
        (
            WireError::Other {
                code: 4242,
                message: "custom".into(),
                data: Some(Box::new(json!({ "k": 1 }))),
            },
            4242,
        ),
    ];
    for (error, expected_code) in cases {
        assert_eq!(error.code(), expected_code);
        let resp = HostResponse {
            id: 1.into(),
            result: Err(error),
        };
        let line = encode_response(&resp);
        let v = wire_json(&line);
        assert_eq!(v["error"]["code"], expected_code);
        assert!(v["error"]["message"].is_string());
        assert!(v.get("result").is_none());
        assert_eq!(response(&line, Method::SessionClose), resp);
    }
}

#[test]
fn error_with_an_unknown_code_is_kept_as_other() {
    let line = json!({ "jsonrpc": "2.0", "id": 1,
                       "error": { "code": -32099, "message": "busy", "data": [1] } })
    .to_string();
    assert_eq!(
        response(line.as_bytes(), Method::SessionNew).result,
        Err(WireError::Other {
            code: -32099,
            message: "busy".into(),
            data: Some(Box::new(json!([1]))),
        })
    );
}

#[test]
fn result_of_the_wrong_shape_is_a_decode_error() {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "result": { "nope": true } }).to_string();
    let HostMessage::Response(raw) = decode_host_message(line.as_bytes()).unwrap() else {
        panic!("expected a response");
    };
    assert!(matches!(
        raw.typed(Method::SessionNew),
        Err(DecodeError::InvalidResult {
            method: Method::SessionNew,
            ..
        })
    ));
}

#[test]
fn response_with_both_result_and_error_is_not_jsonrpc() {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "result": {},
                       "error": { "code": 1, "message": "x" } })
    .to_string();
    assert!(matches!(
        decode_host_message(line.as_bytes()),
        Err(DecodeError::NotJsonRpc(_))
    ));
}

// ---------- notifications ----------

fn envelope() -> shepherd_core::acpp::Envelope {
    draft().seal(
        Seq::new(42),
        Stamp {
            id: EntryId::new("01JBX7Q5W2M9K3ABCDEFGHJKMN"),
            ts: Timestamp::parse("2026-10-29T12:00:00.123Z").unwrap_or_else(|e| panic!("{e}")),
            machine: MachineId::new("m_1"),
        },
    )
}

#[test]
fn log_entry_notification_carries_the_envelope() {
    let note = HostNotification::LogEntry(Box::new(LogEntry {
        envelope: envelope(),
    }));
    let line = encode_host_notification(&note);
    let v = wire_json(&line);
    assert_eq!(v["method"], "_shepherd/log/entry");
    assert!(v.get("id").is_none(), "notifications have no id");
    assert_eq!(v["params"]["envelope"]["seq"], 42);
    assert_eq!(notification(&line), note);
}

#[test]
fn pty_output_notification_carries_base64_bytes() {
    let note = HostNotification::PtyOutput(PtyOutput {
        session: sid("s_1"),
        bytes: vec![0xe2, 0x9c, 0x93, 0xff, b'\n'],
    });
    let line = encode_host_notification(&note);
    let v = wire_json(&line);
    assert_eq!(v["method"], "_shepherd/pty/output");
    assert_eq!(
        v["params"],
        json!({ "session": "s_1", "bytes": "4pyT/wo=" })
    );
    assert_eq!(notification(&line), note);
}

#[test]
fn unknown_notification_is_a_skippable_decode_error() {
    let line = json!({ "jsonrpc": "2.0", "method": "_shepherd/future", "params": {} }).to_string();
    assert_eq!(
        decode_host_message(line.as_bytes()),
        Err(DecodeError::UnknownMethod("_shepherd/future".into()))
    );
}

#[test]
fn pty_output_with_invalid_base64_is_invalid_params() {
    let line = json!({ "jsonrpc": "2.0", "method": "_shepherd/pty/output",
                       "params": { "session": "s_1", "bytes": "abc" } })
    .to_string();
    assert!(matches!(
        decode_host_message(line.as_bytes()),
        Err(DecodeError::InvalidParams { .. })
    ));
}

// ---------- framing ----------

#[test]
fn client_stream_split_at_every_byte_boundary_decodes_unchanged() {
    let mut msgs: Vec<ClientMessage> = one_of_each_request()
        .into_iter()
        .enumerate()
        .map(|(i, body)| {
            ClientMessage::Request(Request {
                id: RequestId::from(i as i64),
                body,
            })
        })
        .collect();
    msgs.extend(
        one_of_each_notification()
            .into_iter()
            .map(ClientMessage::Notification),
    );
    let stream: Vec<u8> = msgs
        .iter()
        .flat_map(|m| match m {
            ClientMessage::Request(r) => encode_request(r),
            ClientMessage::Notification(n) => encode_client_notification(n),
        })
        .collect();
    for split in 0..=stream.len() {
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for part in [&stream[..split], &stream[split..]] {
            d.push(part);
            while let Some(line) = d.next_line() {
                got.push(decode_client_message(&line.unwrap()).unwrap());
            }
        }
        assert_eq!(got, msgs, "split at {split}");
    }
}

#[test]
fn host_stream_split_at_every_byte_boundary_decodes_unchanged() {
    let note = HostNotification::PtyOutput(PtyOutput {
        session: sid("s_1"),
        bytes: b"\r\n$ ".to_vec(),
    });
    let resp = HostResponse {
        id: 1.into(),
        result: Ok(HostResult::Ack),
    };
    let stream = [encode_host_notification(&note), encode_response(&resp)].concat();
    for split in 0..=stream.len() {
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for part in [&stream[..split], &stream[split..]] {
            d.push(part);
            while let Some(line) = d.next_line() {
                got.push(decode_host_message(&line.unwrap()).unwrap());
            }
        }
        assert_eq!(got.len(), 2, "split at {split}");
        assert_eq!(got[0], HostMessage::Notification(note.clone()));
        let HostMessage::Response(raw) = got[1].clone() else {
            panic!("expected a response");
        };
        assert_eq!(raw.typed(Method::SessionClose).unwrap(), resp);
    }
}

// ---------- build identity ----------

fn digest(byte: u8) -> ExeDigest {
    ExeDigest::from_bytes([byte; 32])
}

fn build(byte: u8) -> BuildId {
    BuildId {
        version: "0.1.0".into(),
        digest: digest(byte),
    }
}

#[test]
fn a_digest_is_the_sha256_of_the_bytes_fed_to_it_in_any_chunks() {
    // FIPS 180-2's "abc" vector.
    let expected: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    let mut hasher = DigestHasher::new();
    hasher.update(b"a");
    hasher.update(b"bc");
    assert_eq!(hasher.finish(), ExeDigest::from_bytes(expected));
}

#[test]
fn hello_carries_the_clients_build_and_round_trips() {
    let req = Request {
        id: 1.into(),
        body: ClientRequest::Hello(Hello {
            build: Some(build(7)),
            ..hello()
        }),
    };
    let line = encode_request(&req);
    assert_eq!(
        wire_json(&line)["params"]["build"],
        json!({ "version": "0.1.0", "digest": shepherd_core::wire::base64::encode(&[7; 32]) })
    );
    assert_eq!(request(&line), req);
}

#[test]
fn a_hello_from_a_client_older_than_builds_has_none_and_one_without_leaves_it_off_the_wire() {
    let older = request_line(
        "_shepherd/hello",
        json!({ "version": 1, "machine": "m_7f" }),
    );
    let ClientRequest::Hello(hello_in) = request(&older).body else {
        panic!("not a hello");
    };
    assert_eq!(hello_in.build, None);
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::Hello(hello()),
    });
    assert_eq!(wire_json(&line)["params"].get("build"), None);
}

#[test]
fn hello_ok_carries_the_hosts_build_and_installed_digest_and_round_trips() {
    let ok = negotiate(&hello(), &MachineId::new("m_host"))
        .unwrap_or_else(|e| panic!("{e}"))
        .with_build(Some(build(1)), Some(digest(2)));
    assert_eq!(
        (ok.build.clone(), ok.installed),
        (Some(build(1)), Some(digest(2)))
    );
    let resp = HostResponse {
        id: 1.into(),
        result: Ok(HostResult::Hello(ok)),
    };
    let line = encode_response(&resp);
    let result = &wire_json(&line)["result"];
    assert_eq!(
        result["installed"],
        json!(shepherd_core::wire::base64::encode(&[2; 32]))
    );
    assert_eq!(result["build"]["version"], "0.1.0");
    assert_eq!(response(&line, Method::Hello), resp);
}

#[test]
fn a_hello_ok_from_a_host_older_than_builds_has_no_build_or_installed() {
    let line = json!({ "jsonrpc": "2.0", "id": 1,
                       "result": { "version": PROTOCOL_VERSION, "machine": "m_host" } })
    .to_string();
    let Ok(HostResult::Hello(ok)) = response(line.as_bytes(), Method::Hello).result else {
        panic!("not a hello answer");
    };
    assert_eq!((ok.build, ok.installed), (None, None));
}

#[test]
fn a_digest_that_is_not_32_bytes_is_an_invalid_hello_answer() {
    let short = shepherd_core::wire::base64::encode(&[1; 31]);
    let line = json!({ "jsonrpc": "2.0", "id": 1,
                       "result": { "version": PROTOCOL_VERSION, "machine": "m_host", "installed": short } })
    .to_string();
    let raw = match decode_host_message(line.as_bytes()) {
        Ok(HostMessage::Response(raw)) => raw,
        other => panic!("not a response: {other:?}"),
    };
    assert!(matches!(
        raw.typed(Method::Hello),
        Err(DecodeError::InvalidResult { .. })
    ));
}
