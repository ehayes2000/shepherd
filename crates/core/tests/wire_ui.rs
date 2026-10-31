//! `_shepherd/ui/get` and `_shepherd/ui/apply` on the wire (decision 0022).

use serde_json::{Value, json};
use shepherd_core::acpp::RequestId;
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, PaneId, Ratio, SplitDir};
use shepherd_core::wire::{
    ClientMessage, ClientRequest, DecodeError, HostMessage, HostResponse, HostResult, Method,
    Request, UiApply, UiGet, decode_client_message, decode_host_message, encode_request,
    encode_response,
};

fn wire_json(line: &[u8]) -> Value {
    serde_json::from_slice(line).unwrap_or_else(|e| panic!("{e}"))
}

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
    wire_json(&line)
}

fn typed(line: &[u8], method: Method) -> Result<HostResponse, DecodeError> {
    let Ok(HostMessage::Response(raw)) = decode_host_message(line) else {
        panic!("not a response");
    };
    raw.typed(method)
}

#[test]
fn ui_get_and_ui_apply_are_requests_with_their_own_methods() {
    let get = round_trip(ClientRequest::UiGet(UiGet {}));
    assert_eq!(get["method"], "_shepherd/ui/get");
    assert_eq!(get["params"], json!({}));

    let apply = round_trip(ClientRequest::UiApply(UiApply {
        op: LayoutOp::Split {
            pane: PaneId::new(2),
            dir: SplitDir::Vertical,
            ratio: Ratio::from_permille(250),
            content: Content::Session(SessionId::new("s_1")),
        },
    }));
    assert_eq!(apply["method"], "_shepherd/ui/apply");
    assert_eq!(
        apply["params"],
        json!({ "op": { "kind": "split", "pane": 2, "dir": "vertical", "ratio": 250, "content": { "session": "s_1" } } })
    );
}

#[test]
fn an_apply_without_a_known_op_is_invalid_params() {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "_shepherd/ui/apply", "params": { "op": { "kind": "explode" } } })
        .to_string();
    let error = decode_client_message(line.as_bytes()).unwrap_err();
    assert!(
        matches!(
            error.error,
            shepherd_core::wire::WireError::InvalidParams(_)
        ),
        "{error:?}"
    );
}

#[test]
fn both_are_answered_with_the_layout() {
    let layout = Layout::default();
    for method in [Method::UiGet, Method::UiApply] {
        let resp = HostResponse {
            id: 3.into(),
            result: Ok(HostResult::Layout(layout.clone())),
        };
        let line = encode_response(&resp);
        assert_eq!(
            wire_json(&line)["result"],
            serde_json::to_value(&layout).unwrap()
        );
        assert_eq!(typed(&line, method).unwrap(), resp);
    }
}

#[test]
fn a_malformed_layout_answer_is_a_decode_error() {
    let line =
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "pages": [], "current": 1, "next_id": 2 } })
            .to_string();
    assert!(matches!(
        typed(line.as_bytes(), Method::UiGet),
        Err(DecodeError::InvalidResult {
            method: Method::UiGet,
            ..
        })
    ));
}
