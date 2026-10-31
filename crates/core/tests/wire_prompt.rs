//! `_shepherd/session/prompt` and `_shepherd/session/reply` on the wire (T47, decision 0022).

use serde_json::{Value, json};
use shepherd_core::acpp::{PromptBy, RequestId};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{
    Busy, ClientMessage, ClientRequest, HostMessage, HostResponse, HostResult, Method, ReplyInfo,
    Request, SessionPrompt, SessionReply, SessionReplyOk, WireError, decode_client_message,
    decode_host_message, encode_request, encode_response,
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

fn answered(method: Method, result: Result<HostResult, WireError>) -> HostResponse {
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result,
    });
    let Ok(HostMessage::Response(raw)) = decode_host_message(&line) else {
        panic!("not a response");
    };
    raw.typed(method).unwrap_or_else(|e| panic!("{e}"))
}

fn at(text: &str) -> jiff::Timestamp {
    text.parse().unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_prompt_names_its_session_text_and_who_asked() {
    let sent = round_trip(ClientRequest::SessionPrompt(SessionPrompt {
        session: SessionId::new("s_1"),
        text: "line one\nline two".into(),
        by: Some(PromptBy::Cli),
    }));
    assert_eq!(sent["method"], "_shepherd/session/prompt");
    assert_eq!(
        sent["params"],
        json!({ "session": "s_1", "text": "line one\nline two", "by": "cli" })
    );
    // A client that doesn't say who asked leaves it off.
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "_shepherd/session/prompt",
                       "params": { "session": "s_1", "text": "hi" } })
    .to_string();
    let Ok(ClientMessage::Request(req)) = decode_client_message(line.as_bytes()) else {
        panic!("not a request");
    };
    let ClientRequest::SessionPrompt(prompt) = req.body else {
        panic!("not a prompt");
    };
    assert_eq!(prompt.by, None);
}

#[test]
fn a_reply_request_may_say_since_when() {
    let sent = round_trip(ClientRequest::SessionReply(SessionReply {
        session: SessionId::new("s_1"),
        since: Some(at("2026-10-31T10:00:00.5Z")),
    }));
    assert_eq!(sent["method"], "_shepherd/session/reply");
    assert_eq!(sent["params"]["since"], "2026-10-31T10:00:00.5Z");
    let sent = round_trip(ClientRequest::SessionReply(SessionReply {
        session: SessionId::new("s_1"),
        since: None,
    }));
    assert_eq!(sent["params"], json!({ "session": "s_1" }));
}

#[test]
fn a_reply_is_answered_with_the_text_or_none() {
    let reply = ReplyInfo {
        text: "Done.".into(),
        at: Some(at("2026-10-31T10:00:09Z")),
    };
    for reply in [Some(reply), None] {
        let result = Ok(HostResult::SessionReply(SessionReplyOk {
            reply: reply.clone(),
        }));
        assert_eq!(
            answered(Method::SessionReply, result.clone()).result,
            result
        );
    }
    // A prompt is acked.
    let ack = answered(Method::SessionPrompt, Ok(HostResult::Ack));
    assert_eq!(ack.result, Ok(HostResult::Ack));
}

#[test]
fn a_busy_refusal_says_the_sessions_status() {
    for status in [Busy::Running, Busy::WantsUser] {
        let error = WireError::SessionBusy {
            session: SessionId::new("s_1"),
            status,
        };
        let answer = answered(Method::SessionPrompt, Err(error.clone()));
        assert_eq!(answer.result, Err(error));
    }
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result: Err(WireError::SessionBusy {
            session: SessionId::new("s_1"),
            status: Busy::WantsUser,
        }),
    });
    let sent: Value = serde_json::from_slice(&line).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        sent["error"]["data"],
        json!({ "session": "s_1", "status": "wants-user" })
    );
}
