//! Codex hook translation (`shepherd_core::agents::codex`), checked against the fixtures in `tests/fixtures/codex/`
//! and through the session fold.

use serde_json::{Value, json};
use shepherd_core::acpp::acp::{AcpFrame, ContentBlock, SessionUpdate, ToolCallStatus, ToolKind};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, ShepherdFrame, SourceKind};
use shepherd_core::agents::codex::{CodexTranslation, translate};
use shepherd_core::ids::SessionId;
use shepherd_core::session::{Activity, SessionState, ToolSummary};
use shepherd_testkit::builders::EnvelopeBuilder;

// ---------- fixtures ----------

macro_rules! fx {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/codex/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

const CODEX_SESSION: &str = "019a0000-0000-7000-8000-00000000c0de";

fn all_fixtures() -> Vec<Value> {
    vec![
        fx!("SessionStart.startup"),
        fx!("SessionStart.compact"),
        fx!("SessionStart.clear"),
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("PreToolUse.apply_patch"),
        fx!("PreToolUse.mcp"),
        fx!("PreToolUse.Bash.subagent"),
        fx!("SubagentStart"),
        fx!("Stop"),
        fx!("Stop.continued"),
        fx!("Interrupt"),
        fx!("PreCompact"),
        fx!("PostCompact"),
        fx!("SessionEnd"),
    ]
}

// ---------- helpers ----------

fn state() -> CodexTranslation {
    seeded("1")
}

fn seeded(seed: &str) -> CodexTranslation {
    CodexTranslation::new(SessionId::new("s_test"), seed)
}

fn run(state: &mut CodexTranslation, payloads: &[Value]) -> Vec<FrameDraft> {
    payloads.iter().flat_map(|p| translate(state, p)).collect()
}

fn one(payloads: &[Value]) -> Vec<FrameDraft> {
    run(&mut state(), payloads)
}

/// The drafts the last payload produced, after translating the ones before it.
fn last(payloads: &[Value]) -> Vec<FrameDraft> {
    let mut st = state();
    let Some((last, before)) = payloads.split_last() else {
        panic!("last: needs at least one payload");
    };
    run(&mut st, before);
    translate(&mut st, last)
}

fn methods(drafts: &[FrameDraft]) -> Vec<Option<&str>> {
    drafts.iter().map(|d| d.frame.method()).collect()
}

fn acp(draft: &FrameDraft) -> &AcpFrame {
    match &draft.frame {
        Frame::Acp(m) => m.body(),
        other => panic!("expected an ACP frame, got {other:?}"),
    }
}

fn update(draft: &FrameDraft) -> &SessionUpdate {
    match acp(draft) {
        AcpFrame::Update(n) => &n.update,
        other => panic!("expected session/update, got {other:?}"),
    }
}

fn shepherd(draft: &FrameDraft) -> &ShepherdFrame {
    match &draft.frame {
        Frame::Shepherd(m) => m.body(),
        other => panic!("expected a _shepherd frame, got {other:?}"),
    }
}

fn tool_call(draft: &FrameDraft) -> (String, String, ToolKind, ToolCallStatus) {
    let SessionUpdate::ToolCall(call) = update(draft) else {
        panic!("expected a tool_call, got {:?}", draft.frame);
    };
    (
        call.tool_call_id.clone(),
        call.title.clone(),
        call.kind.clone(),
        call.status.clone(),
    )
}

/// Folds the translation of each payload, returning the activity after each payload.
fn activities(payloads: &[Value]) -> Vec<Activity> {
    let mut st = state();
    let mut log = EnvelopeBuilder::new();
    let mut session = SessionState::default();
    payloads
        .iter()
        .map(|p| {
            for d in translate(&mut st, p) {
                session.apply(&log.envelope(d.dir.clone(), d.frame.raw().clone()));
            }
            session.activity().clone()
        })
        .collect()
}

fn working() -> Activity {
    Activity::Working { tool: None }
}

fn working_on_tests() -> Activity {
    Activity::Working {
        tool: Some(ToolSummary {
            id: "call_bash_1".to_owned(),
            title: "Bash: cargo test".to_owned(),
            kind: ToolKind::Execute,
        }),
    }
}

// ---------- every draft ----------

#[test]
fn every_draft_is_from_the_codex_hook_source_for_the_session() {
    let drafts = one(&all_fixtures());
    assert!(!drafts.is_empty());
    for d in &drafts {
        assert_eq!(d.source.kind, SourceKind::Hook);
        assert_eq!(d.source.agent.as_deref(), Some("codex"));
        assert_eq!(d.session, Some(SessionId::new("s_test")));
    }
}

#[test]
fn every_synthesized_acp_frame_is_marked_and_nothing_else_is() {
    let drafts = one(&all_fixtures());
    assert!(drafts.iter().any(|d| d.frame.is_synthesized()));
    for d in &drafts {
        let is_acp = matches!(d.frame, Frame::Acp(_) | Frame::Response(_));
        assert_eq!(d.frame.is_synthesized(), is_acp, "{:?}", d.frame.raw());
    }
}

#[test]
fn translation_is_deterministic_and_the_state_persists_through_serde() {
    let mut st = state();
    run(
        &mut st,
        &[
            fx!("UserPromptSubmit"),
            fx!("PreToolUse.Bash"),
            fx!("PreCompact"),
        ],
    );
    let mut restored: CodexTranslation =
        serde_json::from_value(serde_json::to_value(&st).unwrap()).unwrap();
    assert_eq!(restored, st);
    let next = [
        fx!("PermissionRequest.Bash"),
        fx!("PostCompact"),
        fx!("Stop"),
    ];
    assert_eq!(run(&mut restored, &next), run(&mut st, &next));
}

#[test]
fn translators_with_different_seeds_never_share_an_id() {
    let session = [
        fx!("UserPromptSubmit"),
        fx!("PermissionRequest.Bash"),
        fx!("PreCompact"),
        fx!("Stop"),
    ];
    let ids = |seed: &str| -> Vec<String> {
        run(&mut seeded(seed), &session)
            .iter()
            .filter_map(|d| {
                let compaction = d.frame.raw()["params"]["update"]["compactionId"].as_str();
                match (d.frame.method(), d.frame.id()) {
                    (Some(_), Some(id)) => Some(id.to_string()),
                    _ => compaction.map(str::to_owned),
                }
            })
            .collect()
    };
    let (a, b) = (ids("e01"), ids("e02"));
    assert_eq!(a.len(), 3);
    for id in &a {
        assert!(!b.contains(id), "{id} repeats across translators");
    }
}

// ---------- prompts and turns ----------

#[test]
fn user_prompt_submit_becomes_a_prompt_request_and_a_user_message_chunk() {
    let drafts = one(&[fx!("UserPromptSubmit")]);
    assert_eq!(
        methods(&drafts),
        vec![Some("session/prompt"), Some("session/update")]
    );
    assert_eq!(drafts[0].dir, Dir::ToAgent);
    let AcpFrame::Prompt(p) = acp(&drafts[0]) else {
        panic!("expected a prompt request");
    };
    assert_eq!(p.session_id, CODEX_SESSION);
    assert!(matches!(&p.prompt[..], [ContentBlock::Text(t)] if t.text == "Run the tests"));
    let SessionUpdate::UserMessageChunk(chunk) = update(&drafts[1]) else {
        panic!("expected a user_message_chunk");
    };
    assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == "Run the tests"));
}

#[test]
fn stop_answers_the_open_prompt_with_end_turn() {
    let mut st = state();
    let prompt = translate(&mut st, &fx!("UserPromptSubmit"));
    let stop = translate(&mut st, &fx!("Stop"));
    assert_eq!(stop.len(), 1);
    let Frame::Response(r) = &stop[0].frame else {
        panic!("expected a response, got {:?}", stop[0].frame);
    };
    assert_eq!(Some(r.id().clone()), prompt[0].frame.id());
    assert_eq!(r.result().unwrap()["stopReason"], "end_turn");
    assert_eq!(stop[0].dir, Dir::FromAgent);
}

#[test]
fn stop_without_an_open_prompt_goes_idle() {
    assert_eq!(
        methods(&one(&[fx!("Stop")])),
        vec![Some("_shepherd/agent/idle")]
    );
}

#[test]
fn interrupt_answers_the_open_prompt_as_cancelled() {
    let mut st = state();
    let prompt = translate(&mut st, &fx!("UserPromptSubmit"));
    let interrupt = translate(&mut st, &fx!("Interrupt"));
    assert_eq!(interrupt.len(), 1);
    let Frame::Response(r) = &interrupt[0].frame else {
        panic!("expected a response, got {:?}", interrupt[0].frame);
    };
    assert_eq!(Some(r.id().clone()), prompt[0].frame.id());
    assert_eq!(r.result().unwrap()["stopReason"], "cancelled");
    // The turn is over: a later Stop has nothing left to answer.
    assert_eq!(
        methods(&translate(&mut st, &fx!("Stop"))),
        vec![Some("_shepherd/agent/idle")]
    );
}

#[test]
fn interrupt_without_an_open_prompt_goes_idle() {
    assert_eq!(
        methods(&one(&[fx!("Interrupt")])),
        vec![Some("_shepherd/agent/idle")]
    );
}

// ---------- tools ----------

#[test]
fn pre_tool_use_becomes_an_in_progress_tool_call_keyed_by_tool_use_id() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Bash")]);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].dir, Dir::FromAgent);
    assert_eq!(
        tool_call(&drafts[0]),
        (
            "call_bash_1".to_owned(),
            "Bash: cargo test".to_owned(),
            ToolKind::Execute,
            ToolCallStatus::InProgress
        )
    );
}

#[test]
fn apply_patch_is_an_edit_titled_by_the_file_it_patches() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PreToolUse.apply_patch")]);
    let (id, title, kind, _) = tool_call(&drafts[0]);
    assert_eq!(id, "call_patch_1");
    assert_eq!(title, "apply_patch: src/lib.rs");
    assert_eq!(kind, ToolKind::Edit);
}

#[test]
fn an_mcp_tool_is_titled_by_its_name() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PreToolUse.mcp")]);
    let (_, title, kind, _) = tool_call(&drafts[0]);
    assert_eq!(title, "mcp__fs__read");
    assert_eq!(kind, ToolKind::default());
}

#[test]
fn post_tool_use_completes_the_tool_call_with_its_response() {
    let drafts = last(&[fx!("PreToolUse.Bash"), fx!("PostToolUse.Bash")]);
    assert_eq!(drafts.len(), 1);
    let SessionUpdate::ToolCallUpdate(u) = update(&drafts[0]) else {
        panic!("expected a tool_call_update");
    };
    assert_eq!(u.tool_call_id, "call_bash_1");
    assert_eq!(u.status, Some(ToolCallStatus::Completed));
    assert_eq!(
        u.raw_output,
        Some(json!("test result: ok. 3 passed; 0 failed"))
    );
}

#[test]
fn permission_request_names_the_pending_tool_even_with_the_added_description() {
    // Codex adds `tool_input.description` to PermissionRequest, which PreToolUse doesn't carry.
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
    ]);
    assert_eq!(methods(&drafts), vec![Some("session/request_permission")]);
    assert_eq!(drafts[0].dir, Dir::FromAgent);
    let AcpFrame::RequestPermission(r) = acp(&drafts[0]) else {
        panic!("expected request_permission");
    };
    assert_eq!(r.tool_call.tool_call_id, "call_bash_1");
    assert_eq!(r.tool_call.title.as_deref(), Some("Bash: cargo test"));
}

#[test]
fn permission_request_for_an_unseen_tool_gets_its_own_tool_call_id() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PermissionRequest.Bash")]);
    let AcpFrame::RequestPermission(r) = acp(&drafts[0]) else {
        panic!("expected request_permission");
    };
    assert_ne!(r.tool_call.tool_call_id, "call_bash_1");
    assert!(!r.tool_call.tool_call_id.is_empty());
}

// ---------- lifecycle ----------

#[test]
fn session_start_becomes_agent_started_with_codexs_session_id() {
    let drafts = one(&[fx!("SessionStart.startup")]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(s.agent, "codex");
    assert_eq!(s.agent_session_id.as_deref(), Some(CODEX_SESSION));
}

#[test]
fn session_start_carries_the_rollout_path_from_the_payload() {
    let drafts = one(&[fx!("SessionStart.startup")]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(
        s.transcript_path.as_deref(),
        Some(
            "/home/me/.codex/sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a0000-0000-7000-8000-00000000c0de.jsonl"
        )
    );
}

#[test]
fn session_start_with_a_null_transcript_path_has_none() {
    // `transcript_path` is a `NullableString` in Codex's schema: null when the session has no rollout file.
    let mut payload = fx!("SessionStart.startup");
    payload["transcript_path"] = Value::Null;
    let drafts = one(&[payload]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(s.transcript_path, None);
    assert_eq!(s.agent_session_id.as_deref(), Some(CODEX_SESSION));
}

#[test]
fn session_start_after_compaction_does_not_restart_the_agent() {
    assert_eq!(
        methods(&one(&[fx!("SessionStart.compact")])),
        vec![Some("_codex/SessionStart")]
    );
}

#[test]
fn session_end_becomes_agent_exited() {
    let drafts = one(&[fx!("SessionEnd")]);
    assert!(matches!(
        shepherd(&drafts[0]),
        ShepherdFrame::AgentExited(_)
    ));
}

// ---------- compaction ----------

#[test]
fn pre_and_post_compact_bracket_one_compaction() {
    let drafts = one(&[fx!("PreCompact"), fx!("PostCompact")]);
    let (SessionUpdate::CompactionUpdate(start), SessionUpdate::CompactionUpdate(end)) =
        (update(&drafts[0]), update(&drafts[1]))
    else {
        panic!("expected two compaction_updates");
    };
    assert_eq!(start.compaction_id, end.compaction_id);
    assert_eq!(start.status.as_str(), "in_progress");
    assert_eq!(end.status.as_str(), "completed");
    assert_eq!(end.summary, None);
}

#[test]
fn post_compact_without_a_pre_compact_is_a_vendor_frame() {
    assert_eq!(
        methods(&one(&[fx!("PostCompact")])),
        vec![Some("_codex/PostCompact")]
    );
}

// ---------- vendor frames ----------

#[test]
fn hooks_without_an_acp_equivalent_become_codex_vendor_frames_with_the_raw_payload() {
    let drafts = one(&[fx!("SubagentStart")]);
    assert_eq!(methods(&drafts), vec![Some("_codex/SubagentStart")]);
    assert_eq!(drafts[0].frame.raw()["params"], fx!("SubagentStart"));
}

#[test]
fn unreadable_payloads_become_unknown_vendor_frames() {
    for payload in [
        json!("not an object"),
        json!({ "session_id": "x" }),
        json!({ "hook_event_name": "has spaces" }),
    ] {
        let drafts = one(std::slice::from_ref(&payload));
        assert_eq!(methods(&drafts), vec![Some("_codex/unknown")], "{payload}");
    }
}

#[test]
fn known_hooks_missing_required_fields_become_vendor_frames() {
    for payload in [
        json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" }),
        json!({ "hook_event_name": "PostToolUse" }),
    ] {
        let name = payload["hook_event_name"].as_str().unwrap().to_owned();
        let drafts = one(std::slice::from_ref(&payload));
        assert_eq!(
            methods(&drafts),
            vec![Some(format!("_codex/{name}").as_str())]
        );
        assert_eq!(drafts[0].frame.raw()["params"], payload);
    }
}

// ---------- through the session fold ----------

#[test]
fn a_full_turn_folds_to_the_expected_activity_sequence() {
    let seen = activities(&[
        fx!("SessionStart.startup"),
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("Stop"),
        fx!("SessionEnd"),
    ]);
    assert_eq!(
        seen,
        vec![
            Activity::Starting,
            working(),
            working_on_tests(),
            Activity::NeedsPermission {
                title: "Bash: cargo test".to_owned()
            },
            working(),
            Activity::Idle,
            Activity::Exited { code: None },
        ]
    );
}

#[test]
fn an_interrupted_turn_folds_to_idle() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("Interrupt"),
    ]);
    assert_eq!(seen, vec![working(), working_on_tests(), Activity::Idle]);
}

#[test]
fn a_turn_a_stop_hook_continues_works_again_until_the_next_stop() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("Stop.continued"),
    ]);
    assert_eq!(
        seen,
        vec![
            working(),
            Activity::Idle,
            working_on_tests(),
            working(),
            Activity::Idle,
        ]
    );
}

#[test]
fn subagent_tool_calls_never_open_a_turn_of_their_own() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Bash.subagent"),
    ]);
    assert_eq!(methods(&drafts), vec![Some("session/update")]);
}

// ---------- which tool a permission names ----------

/// The `toolCallId` of the permission request the last payload produced.
fn permission_tool(payloads: &[Value]) -> String {
    let drafts = last(payloads);
    let Some(AcpFrame::RequestPermission(r)) = drafts
        .iter()
        .find(|d| d.frame.method() == Some("session/request_permission"))
        .map(acp)
    else {
        panic!("no request_permission in {drafts:?}");
    };
    r.tool_call.tool_call_id.clone()
}

fn with(mut payload: Value, key: &str, value: Value) -> Value {
    payload[key] = value;
    payload
}

#[test]
fn a_permission_for_the_same_input_under_another_tool_name_names_no_open_tool() {
    let other = with(fx!("PermissionRequest.Bash"), "tool_name", json!("Shell"));
    let id = permission_tool(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Bash"), other]);
    assert_ne!(id, "call_bash_1");
}

#[test]
fn a_permission_for_the_same_tool_with_other_input_names_no_open_tool() {
    let other = with(
        fx!("PermissionRequest.Bash"),
        "tool_input",
        json!({ "command": "rm -rf target", "description": null }),
    );
    let id = permission_tool(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Bash"), other]);
    assert_ne!(id, "call_bash_1");
}

#[test]
fn a_permission_matching_two_open_tools_names_the_latest() {
    let again = with(fx!("PreToolUse.Bash"), "tool_use_id", json!("call_bash_2"));
    let id = permission_tool(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        again,
        fx!("PermissionRequest.Bash"),
    ]);
    assert_eq!(id, "call_bash_2");
}

#[test]
fn a_new_prompt_forgets_the_tools_of_the_turn_before() {
    let id = permission_tool(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("UserPromptSubmit"),
        fx!("PermissionRequest.Bash"),
    ]);
    assert_ne!(id, "call_bash_1");
}

#[test]
fn a_finished_tool_is_no_longer_named_by_a_permission() {
    let id = permission_tool(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
    ]);
    assert_ne!(id, "call_bash_1");
}

// ---------- what lifecycle hooks reset ----------

#[test]
fn a_new_pre_compact_cancels_a_compaction_that_never_completed() {
    let drafts = one(&[fx!("PreCompact"), fx!("PreCompact")]);
    let updates: Vec<(String, String)> = drafts
        .iter()
        .map(|d| match update(d) {
            SessionUpdate::CompactionUpdate(c) => {
                (c.compaction_id.clone(), c.status.as_str().to_owned())
            }
            other => panic!("expected compaction_update, got {other:?}"),
        })
        .collect();
    assert_eq!(updates.len(), 3, "{updates:?}");
    let first = &updates[0].0;
    assert_eq!(updates[1], (first.clone(), "cancelled".to_owned()));
    assert_ne!(&updates[2].0, first);
    assert_eq!(updates[2].1, "in_progress");
}

/// After `reset`, the turn, its tools and the compaction that were open before it are gone.
fn assert_resets(reset: Value) {
    let mut st = state();
    run(
        &mut st,
        &[
            fx!("UserPromptSubmit"),
            fx!("PreToolUse.Bash"),
            fx!("PreCompact"),
            reset,
        ],
    );
    // No open prompt: Stop only goes idle.
    assert_eq!(
        methods(&translate(&mut st.clone(), &fx!("Stop"))),
        vec![Some("_shepherd/agent/idle")]
    );
    // No open compaction: PostCompact has nothing to complete.
    assert_eq!(
        methods(&translate(&mut st.clone(), &fx!("PostCompact"))),
        vec![Some("_codex/PostCompact")]
    );
    // No open tool: a permission for it gets its own id.
    let drafts = translate(&mut st, &fx!("PermissionRequest.Bash"));
    let Some(AcpFrame::RequestPermission(r)) = drafts
        .iter()
        .find(|d| d.frame.method() == Some("session/request_permission"))
        .map(acp)
    else {
        panic!("no request_permission in {drafts:?}");
    };
    assert_ne!(r.tool_call.tool_call_id, "call_bash_1");
}

#[test]
fn session_start_forgets_the_turn_tools_and_compaction_in_progress() {
    assert_resets(fx!("SessionStart.startup"));
}

#[test]
fn session_end_forgets_the_turn_tools_and_compaction_in_progress() {
    assert_resets(fx!("SessionEnd"));
}

#[test]
fn a_hook_without_a_session_id_uses_the_one_from_session_start() {
    let mut prompt = fx!("UserPromptSubmit");
    prompt.as_object_mut().unwrap().remove("session_id");
    let drafts = last(&[fx!("SessionStart.startup"), prompt]);
    let AcpFrame::Prompt(p) = acp(&drafts[0]) else {
        panic!("expected a prompt request");
    };
    assert_eq!(p.session_id, CODEX_SESSION);
}
