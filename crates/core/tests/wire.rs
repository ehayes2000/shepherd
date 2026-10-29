//! Client↔host wire messages, through the public `shepherd_core::wire` API.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use shepherd_core::acpp::{
    Dir, EntryId, Frame, FrameDraft, RequestId, Seq, Source, SourceKind, Stamp, Timestamp,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    ClientRequest, DecodeError, Decoder, Hello, HelloOk, HookFrame, HostMessage, HostNotification,
    HostResponse, HostResult, LogEntry, Method, PROTOCOL_VERSION, PtyOutput, Request, SessionClose,
    SessionInput, SessionNew, SessionNewOk, SessionRename, SessionResize, Subscribe, SubscribeFrom,
    WireError, code, decode_host_message, decode_request, encode_notification, encode_request,
    encode_response, negotiate,
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

fn one_of_each_request() -> Vec<ClientRequest> {
    vec![
        ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_1"),
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
        }),
        ClientRequest::SessionInput(SessionInput {
            session: sid("s_1"),
            data: vec![0x1b, b'[', b'A', 0xff, 0x00],
        }),
        ClientRequest::SessionResize(SessionResize {
            session: sid("s_1"),
            cols: 120,
            rows: 40,
        }),
        ClientRequest::Subscribe(Subscribe {
            from: SubscribeFrom {
                host: Some(Seq::new(3)),
                sessions: BTreeMap::from([(sid("s_1"), Seq::new(7)), (sid("s_2"), Seq::new(1))]),
            },
        }),
        ClientRequest::Subscribe(Subscribe::default()),
        ClientRequest::HookFrame(Box::new(HookFrame { draft: draft() })),
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

fn rejected(line: &[u8]) -> (Option<RequestId>, WireError) {
    let Err(err) = decode_request(line) else {
        panic!("expected the request to be rejected");
    };
    (err.id, err.error)
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
        assert_eq!(decode_request(&encode_request(&req)).unwrap(), req);
    }
}

#[test]
fn requests_are_jsonrpc_with_shepherd_method_names() {
    let expected = [
        "_shepherd/hello",
        "_shepherd/session/new",
        "_shepherd/session/new",
        "_shepherd/session/rename",
        "_shepherd/session/close",
        "_shepherd/session/input",
        "_shepherd/session/resize",
        "_shepherd/subscribe",
        "_shepherd/subscribe",
        "_shepherd/hook/frame",
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
        }),
    });
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "version": 1, "machine": "m_7f" })
    );
}

#[test]
fn input_bytes_are_base64_on_the_wire() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::SessionInput(SessionInput {
            session: sid("s_1"),
            data: b"foobar".to_vec(),
        }),
    });
    assert_eq!(
        wire_json(&line)["params"],
        json!({ "session": "s_1", "data": "Zm9vYmFy" })
    );
}

#[test]
fn input_with_invalid_base64_is_rejected_as_invalid_params() {
    let line = request_line(
        "_shepherd/session/input",
        json!({ "session": "s_1", "data": "Zm9v!" }),
    );
    let (id, error) = rejected(&line);
    assert_eq!(id, Some(9.into()));
    assert!(matches!(error, WireError::InvalidParams(_)), "{error:?}");
}

#[test]
fn resize_params_are_cols_and_rows() {
    let line = request_line(
        "_shepherd/session/resize",
        json!({ "session": "s_1", "cols": 80, "rows": 24 }),
    );
    assert_eq!(
        decode_request(&line).unwrap().body,
        ClientRequest::SessionResize(SessionResize {
            session: sid("s_1"),
            cols: 80,
            rows: 24
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
        decode_request(&line).unwrap().body,
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
        decode_request(&line).unwrap().body,
        ClientRequest::Subscribe(Subscribe::default())
    );
}

#[test]
fn session_new_without_params_uses_defaults() {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "_shepherd/session/new" }).to_string();
    assert_eq!(
        decode_request(line.as_bytes()).unwrap().body,
        ClientRequest::SessionNew(SessionNew::default())
    );
}

#[test]
fn hook_frame_carries_the_draft_frame_verbatim() {
    let line = encode_request(&Request {
        id: 1.into(),
        body: ClientRequest::HookFrame(Box::new(HookFrame { draft: draft() })),
    });
    let v = wire_json(&line);
    assert_eq!(
        v["params"]["draft"]["frame"],
        json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": { "extra": [1, 2] } })
    );
    assert_eq!(v["params"]["draft"]["source"]["kind"], "hook");
}

#[test]
fn unknown_request_fields_are_ignored() {
    let line = request_line(
        "_shepherd/session/close",
        json!({ "session": "s_1", "force": true }),
    );
    assert_eq!(
        decode_request(&line).unwrap().body,
        ClientRequest::SessionClose(SessionClose {
            session: sid("s_1")
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
}

#[test]
fn malformed_json_is_a_parse_error_answered_with_a_null_id() {
    let (id, error) = rejected(b"{\"jsonrpc\":");
    assert_eq!(id, None);
    assert!(matches!(error, WireError::Parse(_)), "{error:?}");

    let resp = decode_request(b"{\"jsonrpc\":")
        .unwrap_err()
        .into_response();
    let v = wire_json(&encode_response(&resp));
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["code"], code::PARSE_ERROR);
}

#[test]
fn request_without_an_id_is_invalid() {
    let line = json!({ "jsonrpc": "2.0", "method": "_shepherd/session/close",
                       "params": { "session": "s_1" } })
    .to_string();
    let (id, error) = rejected(line.as_bytes());
    assert_eq!(id, None);
    assert!(matches!(error, WireError::InvalidRequest(_)), "{error:?}");
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
    let line = request_line(
        "_shepherd/session/resize",
        json!({ "session": "s_1", "cols": -1 }),
    );
    let (id, error) = rejected(&line);
    assert_eq!(id, Some(9.into()));
    assert!(matches!(error, WireError::InvalidParams(_)), "{error:?}");
}

// ---------- hello ----------

#[test]
fn hello_with_matching_version_returns_host_version_and_machine() {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("m_client"),
    };
    assert_eq!(
        negotiate(&hello, &MachineId::new("m_host")),
        Ok(HelloOk {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("m_host"),
        })
    );
}

#[test]
fn hello_with_other_version_gets_a_typed_version_mismatch_response() {
    let hello = Hello {
        version: PROTOCOL_VERSION + 1,
        machine: MachineId::new("m_client"),
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
    let line = encode_notification(&note);
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
    let line = encode_notification(&note);
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
fn encoded_requests_split_at_every_byte_boundary_decode_unchanged() {
    let reqs: Vec<Request> = one_of_each_request()
        .into_iter()
        .enumerate()
        .map(|(i, body)| Request {
            id: RequestId::from(i as i64),
            body,
        })
        .collect();
    let stream: Vec<u8> = reqs.iter().flat_map(encode_request).collect();
    for split in 0..=stream.len() {
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for part in [&stream[..split], &stream[split..]] {
            d.push(part);
            while let Some(line) = d.next_line() {
                got.push(decode_request(&line.unwrap()).unwrap());
            }
        }
        assert_eq!(got, reqs, "split at {split}");
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
    let stream = [encode_notification(&note), encode_response(&resp)].concat();
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
