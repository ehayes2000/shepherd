//! `_shepherd/session/screen` on the wire (T52).

use serde_json::{Value, json};
use shepherd_core::acpp::RequestId;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{
    ClientMessage, ClientRequest, HostMessage, HostResponse, HostResult, Method, Request,
    SessionScreen, SessionScreenOk, WireError, decode_client_message, decode_host_message,
    encode_request, encode_response,
};

fn round_trip(body: ClientRequest) -> Value {
    let req = Request {
        id: RequestId::from(1),
        body,
    };
    let line = encode_request(&req);
    assert_eq!(
        decode_client_message(&line).unwrap_or_else(|e| panic!("{e}")),
        ClientMessage::Request(req)
    );
    serde_json::from_slice(&line).unwrap_or_else(|e| panic!("{e}"))
}

fn answered(result: Result<HostResult, WireError>) -> (Value, HostResponse) {
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result,
    });
    let sent = serde_json::from_slice(&line).unwrap_or_else(|e| panic!("{e}"));
    let Ok(HostMessage::Response(raw)) = decode_host_message(&line) else {
        panic!("not a response");
    };
    (
        sent,
        raw.typed(Method::SessionScreen)
            .unwrap_or_else(|e| panic!("{e}")),
    )
}

#[test]
fn a_screen_request_names_its_session_and_may_ask_for_scrollback() {
    let sent = round_trip(ClientRequest::SessionScreen(SessionScreen {
        session: SessionId::new("s_1"),
        lines: Some(50),
    }));
    assert_eq!(sent["method"], "_shepherd/session/screen");
    assert_eq!(sent["params"], json!({ "session": "s_1", "lines": 50 }));
    let sent = round_trip(ClientRequest::SessionScreen(SessionScreen {
        session: SessionId::new("s_1"),
        lines: None,
    }));
    assert_eq!(sent["params"], json!({ "session": "s_1" }));
}

#[test]
fn a_screen_is_answered_with_its_size_and_lines() {
    let ok = SessionScreenOk {
        cols: 80,
        rows: 2,
        scrollback: 1,
        lines: vec!["$ ls".into(), "漢字 file".into(), String::new()],
        truncated: false,
    };
    let (sent, answer) = answered(Ok(HostResult::SessionScreen(ok.clone())));
    assert_eq!(answer.result, Ok(HostResult::SessionScreen(ok)));
    assert_eq!(
        sent["result"],
        json!({ "cols": 80, "rows": 2, "scrollback": 1, "lines": ["$ ls", "漢字 file", ""],
                "truncated": false })
    );
}
