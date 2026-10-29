//! The per-session activity fold, through the public `shepherd_core::session` API.

use proptest::prelude::*;
use serde_json::{Value, json};
use shepherd_core::acpp::acp::ToolKind;
use shepherd_core::acpp::{Dir, Seq};
use shepherd_core::session::{Activity, SessionState, ToolSummary};
use shepherd_testkit::builders::EnvelopeBuilder;

// ---------- frames ----------

fn prompt(id: i64, text: &str) -> (Dir, Value) {
    (
        Dir::ToAgent,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [{ "type": "text", "text": text }] } }),
    )
}

fn prompt_done(id: i64) -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } }),
    )
}

fn prompt_failed(id: i64) -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": "boom" } }),
    )
}

fn update(update: Value) -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "method": "session/update",
                "params": { "sessionId": "a1", "update": update } }),
    )
}

fn tool_call(tool_id: &str, title: &str) -> (Dir, Value) {
    update(
        json!({ "sessionUpdate": "tool_call", "toolCallId": tool_id, "title": title,
                   "kind": "execute", "status": "in_progress" }),
    )
}

fn tool_update(tool_id: &str, status: &str) -> (Dir, Value) {
    update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool_id, "status": status }))
}

fn permission(id: i64, tool_id: &str, title: &str) -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
                "params": { "sessionId": "a1", "toolCall": { "toolCallId": tool_id, "title": title },
                            "options": [] } }),
    )
}

fn permission_answered(id: i64) -> (Dir, Value) {
    (
        Dir::ToAgent,
        json!({ "jsonrpc": "2.0", "id": id,
                "result": { "outcome": { "outcome": "selected", "optionId": "allow" } } }),
    )
}

fn shepherd(method: &str, params: Value) -> (Dir, Value) {
    (
        Dir::Internal,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn started() -> (Dir, Value) {
    shepherd(
        "_shepherd/agent/started",
        json!({ "agent": "claude-code", "command": "claude" }),
    )
}

fn idle() -> (Dir, Value) {
    shepherd("_shepherd/agent/idle", json!({}))
}

fn exited(code: i32) -> (Dir, Value) {
    shepherd("_shepherd/agent/exited", json!({ "code": code }))
}

fn unknown() -> (Dir, Value) {
    (
        Dir::FromAgent,
        json!({ "jsonrpc": "2.0", "method": "_vendor/thing", "params": { "x": 1 } }),
    )
}

// ---------- helpers ----------

type Step = (Dir, Value);

fn fold(steps: &[(Dir, Value)]) -> SessionState {
    let mut log = EnvelopeBuilder::new();
    let mut state = SessionState::default();
    for (dir, frame) in steps {
        state.apply(&log.envelope(dir.clone(), frame.clone()));
    }
    state
}

fn working_on(tool_id: &str, title: &str) -> Activity {
    Activity::Working {
        tool: Some(ToolSummary {
            id: tool_id.to_owned(),
            title: title.to_owned(),
            kind: ToolKind::Execute,
        }),
    }
}

fn working() -> Activity {
    Activity::Working { tool: None }
}

fn needs_permission(title: &str) -> Activity {
    Activity::NeedsPermission {
        title: title.to_owned(),
    }
}

// ---------- transitions ----------

#[test]
fn activity_transitions() {
    let cases: Vec<(&str, Vec<Step>, Activity)> = vec![
        ("no frames is starting", vec![], Activity::Starting),
        (
            "agent started stays starting",
            vec![started()],
            Activity::Starting,
        ),
        (
            "prompt request starts working",
            vec![started(), prompt(1, "hi")],
            working(),
        ),
        (
            "tool call works on that tool",
            vec![prompt(1, "hi"), tool_call("t1", "ls")],
            working_on("t1", "ls"),
        ),
        (
            "completed tool update clears the tool",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "ls"),
                tool_update("t1", "completed"),
            ],
            working(),
        ),
        (
            "failed tool update clears the tool",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "ls"),
                tool_update("t1", "failed"),
            ],
            working(),
        ),
        (
            "in-progress tool update keeps the tool",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "ls"),
                tool_update("t1", "in_progress"),
            ],
            working_on("t1", "ls"),
        ),
        (
            "update for another tool keeps the current tool",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "ls"),
                tool_update("t0", "completed"),
            ],
            working_on("t1", "ls"),
        ),
        (
            "permission request needs permission",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "rm -rf"),
                permission(7, "t1", "Run rm -rf?"),
            ],
            needs_permission("Run rm -rf?"),
        ),
        (
            "permission request without a title uses the tool's title",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "rm -rf"),
                (
                    Dir::FromAgent,
                    json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
                            "params": { "sessionId": "a1", "toolCall": { "toolCallId": "t1" } } }),
                ),
            ],
            needs_permission("rm -rf"),
        ),
        (
            "later tool update clears permission",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "rm -rf"),
                permission(7, "t1", "Run rm -rf?"),
                tool_update("t1", "in_progress"),
            ],
            working_on("t1", "rm -rf"),
        ),
        (
            "later completed tool update clears permission and tool",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "rm -rf"),
                permission(7, "t1", "Run rm -rf?"),
                tool_update("t1", "completed"),
            ],
            working(),
        ),
        (
            "later tool call clears permission",
            vec![
                prompt(1, "hi"),
                permission(7, "t1", "Run rm -rf?"),
                tool_call("t2", "ls"),
            ],
            working_on("t2", "ls"),
        ),
        (
            "permission response clears permission",
            vec![
                prompt(1, "hi"),
                tool_call("t1", "rm -rf"),
                permission(7, "t1", "Run rm -rf?"),
                permission_answered(7),
            ],
            working_on("t1", "rm -rf"),
        ),
        (
            "prompt response clears permission to idle",
            vec![
                prompt(1, "hi"),
                permission(7, "t1", "Run rm -rf?"),
                prompt_done(1),
            ],
            Activity::Idle,
        ),
        (
            "prompt response is idle",
            vec![prompt(1, "hi"), tool_call("t1", "ls"), prompt_done(1)],
            Activity::Idle,
        ),
        (
            "prompt error response is idle",
            vec![prompt(1, "hi"), prompt_failed(1)],
            Activity::Idle,
        ),
        (
            "response with the prompt's direction does not correlate",
            vec![
                prompt(1, "hi"),
                (
                    Dir::ToAgent,
                    json!({ "jsonrpc": "2.0", "id": 1, "result": { "stopReason": "end_turn" } }),
                ),
            ],
            working(),
        ),
        (
            "agent idle is idle",
            vec![started(), idle()],
            Activity::Idle,
        ),
        (
            "agent idle mid-turn is idle",
            vec![prompt(1, "hi"), tool_call("t1", "ls"), idle()],
            Activity::Idle,
        ),
        (
            "agent exited is exited",
            vec![prompt(1, "hi"), exited(3)],
            Activity::Exited { code: Some(3) },
        ),
        (
            "exited ignores late agent frames",
            vec![prompt(1, "hi"), exited(0), tool_call("t1", "ls"), idle()],
            Activity::Exited { code: Some(0) },
        ),
        (
            "agent started after exit is starting",
            vec![exited(0), started()],
            Activity::Starting,
        ),
        (
            "new prompt after idle works",
            vec![prompt(1, "a"), prompt_done(1), prompt(2, "b")],
            working(),
        ),
    ];
    for (name, steps, expected) in cases {
        assert_eq!(fold(&steps).activity(), &expected, "case: {name}");
    }
}

#[test]
fn unknown_frames_do_not_change_state() {
    let prefixes: Vec<Vec<(Dir, Value)>> = vec![
        vec![],
        vec![prompt(1, "hi")],
        vec![prompt(1, "hi"), tool_call("t1", "ls")],
        vec![prompt(1, "hi"), permission(7, "t1", "ok?")],
        vec![prompt(1, "hi"), prompt_done(1)],
        vec![exited(1)],
    ];
    let noise = [
        unknown(),
        (
            Dir::ToAgent,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "_vendor/ask", "params": {} }),
        ),
        update(json!({ "sessionUpdate": "agent_message_chunk",
                       "content": { "type": "text", "text": "hm" } })),
        (
            Dir::FromAgent,
            json!({ "jsonrpc": "2.0", "id": 99, "result": {} }),
        ),
        shepherd("_shepherd/session/renamed", json!({ "name": "x" })),
    ];
    for prefix in prefixes {
        let before = fold(&prefix);
        for n in &noise {
            let mut steps = prefix.clone();
            steps.push(n.clone());
            assert_eq!(fold(&steps), before, "noise {n:?} after {prefix:?}");
        }
    }
}

// ---------- attention ----------

#[test]
fn attention_when_permission_is_needed() {
    let state = fold(&[prompt(1, "hi"), permission(7, "t1", "ok?")]);
    assert!(state.attention(Seq::new(100)));
}

#[test]
fn no_attention_while_working_or_starting() {
    assert!(!fold(&[]).attention(Seq::new(0)));
    assert!(!fold(&[started()]).attention(Seq::new(0)));
    assert!(!fold(&[prompt(1, "hi"), tool_call("t1", "ls")]).attention(Seq::new(0)));
}

#[test]
fn attention_for_idle_reached_after_last_seen() {
    // prompt is seq 1, the response (idle) is seq 2.
    let state = fold(&[prompt(1, "hi"), prompt_done(1)]);
    assert!(state.attention(Seq::new(1)));
    assert!(!state.attention(Seq::new(2)));
    assert!(!state.attention(Seq::new(3)));
}

#[test]
fn repeated_idle_keeps_the_seq_it_was_first_reached_at() {
    // idle reached at seq 2 by the prompt response, then the agent/idle frame at seq 3 says the same thing.
    let state = fold(&[prompt(1, "hi"), prompt_done(1), idle()]);
    assert!(!state.attention(Seq::new(2)));
}

#[test]
fn idle_after_a_new_turn_needs_attention_again() {
    let state = fold(&[prompt(1, "a"), prompt_done(1), prompt(2, "b"), idle()]);
    assert!(state.attention(Seq::new(3)));
    assert!(!state.attention(Seq::new(4)));
}

#[test]
fn no_attention_after_exit() {
    let state = fold(&[prompt(1, "hi"), prompt_done(1), exited(0)]);
    assert!(!state.attention(Seq::new(0)));
}

// ---------- turns ----------

#[test]
fn turn_count_counts_prompts() {
    assert_eq!(fold(&[]).turns(), 0);
    let state = fold(&[prompt(1, "a"), prompt_done(1), prompt(2, "b")]);
    assert_eq!(state.turns(), 2);
}

#[test]
fn last_prompt_is_the_latest_prompt_text() {
    assert_eq!(fold(&[]).last_prompt(), None);
    let state = fold(&[
        prompt(1, "fix the login"),
        prompt_done(1),
        prompt(2, "now add tests"),
    ]);
    assert_eq!(state.last_prompt(), Some("now add tests"));
}

#[test]
fn last_prompt_joins_text_blocks_and_skips_others() {
    let state = fold(&[(
        Dir::ToAgent,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": { "sessionId": "a1", "prompt": [
                    { "type": "text", "text": "look at" },
                    { "type": "image", "data": "..", "mimeType": "image/png" },
                    { "type": "text", "text": "this" } ] } }),
    )]);
    assert_eq!(state.last_prompt(), Some("look at\nthis"));
}

// ---------- property ----------

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        "[a-z_]{0,8}".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(
                prop_oneof![
                    Just("sessionId".to_owned()),
                    Just("sessionUpdate".to_owned()),
                    Just("toolCallId".to_owned()),
                    Just("title".to_owned()),
                    Just("status".to_owned()),
                    Just("toolCall".to_owned()),
                    Just("update".to_owned()),
                    Just("prompt".to_owned()),
                    Just("code".to_owned()),
                    "[a-z]{1,6}",
                ],
                inner,
                0..5
            )
            .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn arb_method() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("session/prompt".to_owned()),
        Just("session/update".to_owned()),
        Just("session/request_permission".to_owned()),
        Just("session/cancel".to_owned()),
        Just("_shepherd/agent/started".to_owned()),
        Just("_shepherd/agent/idle".to_owned()),
        Just("_shepherd/agent/exited".to_owned()),
        "[a-z_/]{1,12}",
    ]
}

fn arb_id() -> impl Strategy<Value = Value> {
    prop_oneof![
        (0i64..4).prop_map(|n| json!(n)),
        "[ab]".prop_map(Value::String),
        Just(Value::Null),
    ]
}

fn arb_frame() -> impl Strategy<Value = (Dir, Value)> {
    let dir = prop_oneof![
        Just(Dir::ToAgent),
        Just(Dir::FromAgent),
        Just(Dir::Internal)
    ];
    let request = (arb_id(), arb_method(), arb_json())
        .prop_map(|(id, m, p)| json!({ "jsonrpc": "2.0", "id": id, "method": m, "params": p }));
    let notification = (arb_method(), arb_json())
        .prop_map(|(m, p)| json!({ "jsonrpc": "2.0", "method": m, "params": p }));
    let result = (arb_id(), arb_json())
        .prop_map(|(id, r)| json!({ "jsonrpc": "2.0", "id": id, "result": r }));
    let error = (arb_id(), arb_json())
        .prop_map(|(id, e)| json!({ "jsonrpc": "2.0", "id": id, "error": e }));
    (dir, prop_oneof![request, notification, result, error])
}

proptest! {
    #[test]
    fn folding_arbitrary_frames_never_panics(steps in prop::collection::vec(arb_frame(), 0..32)) {
        let state = fold(&steps);
        let _ = state.attention(Seq::new(0));
        let _ = (state.activity(), state.turns(), state.last_prompt());
    }
}
