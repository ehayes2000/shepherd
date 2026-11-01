//! `_shepherd/session/answer` on the wire (T51).

use std::num::NonZeroU32;

use serde_json::{Value, json};
use shepherd_core::acpp::{Choice, PromptBy, RequestId, Seq};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{
    ClientMessage, ClientRequest, HostMessage, HostResponse, HostResult, Method, Request,
    SessionAnswer, Status, WireError, decode_client_message, decode_host_message, encode_request,
    encode_response,
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

fn decoded(params: Value) -> Result<ClientMessage, WireError> {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "_shepherd/session/answer",
                       "params": params })
    .to_string();
    decode_client_message(line.as_bytes()).map_err(|e| e.error)
}

#[test]
fn an_answer_names_its_session_a_word_or_an_options_number_and_who_asked() {
    for (choice, sent) in [
        (Choice::Yes, json!("yes")),
        (Choice::Always, json!("always")),
        (Choice::No, json!("no")),
        (Choice::Option(NonZeroU32::MIN.saturating_add(1)), json!(2)),
    ] {
        let wire = round_trip(ClientRequest::SessionAnswer(SessionAnswer {
            session: SessionId::new("s_1"),
            choice,
            asked: Some(Seq::new(7)),
            by: Some(PromptBy::Cli),
        }));
        assert_eq!(wire["method"], "_shepherd/session/answer");
        assert_eq!(
            wire["params"],
            json!({ "session": "s_1", "choice": sent, "asked": 7, "by": "cli" })
        );
    }
    let Ok(ClientMessage::Request(req)) = decoded(json!({ "session": "s_1", "choice": "no" }))
    else {
        panic!("not a request");
    };
    let ClientRequest::SessionAnswer(answer) = req.body else {
        panic!("not an answer");
    };
    assert_eq!((answer.by, answer.asked), (None, None));
}

#[test]
fn a_choice_that_is_no_choice_is_invalid_params() {
    for choice in [json!(0), json!("maybe"), json!(-1), json!(null)] {
        assert!(
            matches!(
                decoded(json!({ "session": "s_1", "choice": choice })),
                Err(WireError::InvalidParams(_))
            ),
            "{choice}"
        );
    }
}

#[test]
fn nothing_to_answer_says_the_sessions_status() {
    for status in [
        Status::Running,
        Status::Idle,
        Status::WantsUser,
        Status::Exited,
    ] {
        let error = WireError::NothingAsked {
            session: SessionId::new("s_1"),
            status,
        };
        let line = encode_response(&HostResponse {
            id: 4.into(),
            result: Err(error.clone()),
        });
        let Ok(HostMessage::Response(raw)) = decode_host_message(&line) else {
            panic!("not a response");
        };
        let typed = raw
            .typed(Method::SessionAnswer)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(typed.result, Err(error));
    }
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result: Err(WireError::NothingAsked {
            session: SessionId::new("s_1"),
            status: Status::WantsUser,
        }),
    });
    let sent: Value = serde_json::from_slice(&line).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sent["error"]["code"], 1003);
    assert_eq!(
        sent["error"]["data"],
        json!({ "session": "s_1", "status": "wants-user" })
    );
    // An answer is acked.
    let line = encode_response(&HostResponse {
        id: 4.into(),
        result: Ok(HostResult::Ack),
    });
    let Ok(HostMessage::Response(raw)) = decode_host_message(&line) else {
        panic!("not a response");
    };
    assert_eq!(
        raw.typed(Method::SessionAnswer)
            .unwrap_or_else(|e| panic!("{e}"))
            .result,
        Ok(HostResult::Ack)
    );
}
