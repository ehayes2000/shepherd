//! Claude Code hook translation (`shepherd_core::agents::claude`), checked against the recorded fixtures in
//! `tests/fixtures/claude/` and through the session fold.

use proptest::prelude::*;
use serde_json::{Value, json};
use shepherd_core::acpp::acp::{AcpFrame, ContentBlock, SessionUpdate, ToolCallStatus, ToolKind};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, ShepherdFrame, SourceKind};
use shepherd_core::agents::claude::{ClaudeTranslation, translate};
use shepherd_core::ids::SessionId;
use shepherd_core::session::{Activity, SessionState, ToolSummary};
use shepherd_testkit::builders::EnvelopeBuilder;

// ---------- fixtures ----------

macro_rules! fx {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/claude/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

fn all_fixtures() -> Vec<Value> {
    vec![
        fx!("SessionStart.startup"),
        fx!("SessionStart.compact"),
        fx!("SessionStart.clear"),
        fx!("UserPromptSubmit"),
        fx!("UserPromptSubmit.second"),
        fx!("PreToolUse.Bash"),
        fx!("PreToolUse.Read"),
        fx!("PermissionRequest.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("PostToolUseFailure.Bash"),
        fx!("PermissionDenied.Bash"),
        fx!("Notification.permission_prompt"),
        fx!("Notification.idle_prompt"),
        fx!("Notification.auth_success"),
        fx!("Stop"),
        fx!("StopFailure"),
        fx!("PreCompact"),
        fx!("PostCompact"),
        fx!("SessionEnd.other"),
        fx!("SessionEnd.clear"),
        fx!("CwdChanged"),
        fx!("Stop.continued"),
        fx!("PreToolUse.Grep.subagent"),
        fx!("PermissionRequest.Grep.subagent"),
        fx!("PostToolUse.Grep.subagent"),
        fx!("PreToolUse.AskUserQuestion.subagent"),
        fx!("PreToolUse.Agent"),
        fx!("PostToolUse.Agent"),
        fx!("PostToolUse.Agent.background"),
        fx!("PostToolUseFailure.Agent"),
        fx!("PreToolUse.Agent.second"),
        fx!("PostToolUse.Agent.second"),
        fx!("SubagentStart.second"),
        fx!("PreToolUse.Agent.default"),
        fx!("SubagentStart.default"),
        fx!("SubagentStart"),
        fx!("SubagentStop"),
        fx!("SubagentStart.internal"),
        fx!("SubagentStop.internal"),
        fx!("PreToolUse.Agent.hostile"),
        fx!("SubagentStart.hostile"),
    ]
}

// ---------- helpers ----------

fn state() -> ClaudeTranslation {
    seeded("1")
}

fn seeded(seed: &str) -> ClaudeTranslation {
    ClaudeTranslation::new(SessionId::new("s_test"), seed)
}

/// Whether a draft is a synthesized continuation of a turn a Stop hook kept going.
fn is_continuation(d: &FrameDraft) -> bool {
    d.frame.method() == Some("session/prompt")
        && d.frame.raw()["params"]["_meta"]["shepherd"]["continuation"] == json!(true)
}

/// Translates `payloads` in order with one translation state, returning all drafts.
fn run(state: &mut ClaudeTranslation, payloads: &[Value]) -> Vec<FrameDraft> {
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

/// Folds drafts through a fresh session log, returning the activity after each one.
fn fold(drafts: &[FrameDraft]) -> (SessionState, Vec<Activity>) {
    let mut log = EnvelopeBuilder::new();
    let mut session = SessionState::default();
    let mut seen = Vec::new();
    for d in drafts {
        session.apply(&log.envelope(d.dir.clone(), d.frame.raw().clone()));
        seen.push(session.activity().clone());
    }
    (session, seen)
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

fn working_on_bash() -> Activity {
    Activity::Working {
        tool: Some(ToolSummary {
            id: "toolu_01ABC123".to_owned(),
            title: "Bash: npm test".to_owned(),
            kind: ToolKind::Execute,
        }),
    }
}

fn needs_permission(title: &str) -> Activity {
    Activity::NeedsPermission {
        title: title.to_owned(),
    }
}

// ---------- every draft ----------

#[test]
fn every_draft_is_from_the_claude_code_hook_source_for_the_session() {
    let drafts = one(&all_fixtures());
    assert!(!drafts.is_empty());
    for d in &drafts {
        assert_eq!(d.source.kind, SourceKind::Hook);
        assert_eq!(d.source.agent.as_deref(), Some("claude-code"));
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

// ---------- prompts ----------

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
    assert_eq!(p.session_id, "abc123");
    let text = "Write a function to calculate the factorial of a number";
    assert!(matches!(&p.prompt[..], [ContentBlock::Text(t)] if t.text == text));
    assert!(drafts[0].frame.id().is_some());

    assert_eq!(drafts[1].dir, Dir::FromAgent);
    let SessionUpdate::UserMessageChunk(chunk) = update(&drafts[1]) else {
        panic!("expected a user_message_chunk");
    };
    assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == text));
}

#[test]
fn stop_answers_the_open_prompt_with_end_turn_from_the_agent() {
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
    let drafts = one(&[fx!("Stop")]);
    assert_eq!(methods(&drafts), vec![Some("_shepherd/agent/idle")]);
}

#[test]
fn prompt_ids_are_distinct_per_turn_and_deterministic_from_the_state() {
    let turns = [
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("UserPromptSubmit.second"),
        fx!("Stop"),
    ];
    let a = one(&turns);
    let b = one(&turns);
    assert_eq!(a, b);

    let first = a[0].frame.id();
    let second = a[3].frame.id();
    assert_eq!(a[3].frame.method(), Some("session/prompt"));
    assert_ne!(first, second);
    assert_eq!(a[2].frame.id(), first);
    assert_eq!(a[5].frame.id(), second);
}

#[test]
fn translation_state_survives_a_round_trip_between_calls() {
    let mut st = state();
    translate(&mut st, &fx!("UserPromptSubmit"));
    let carried = st.clone();
    assert_eq!(
        translate(&mut st, &fx!("Stop")),
        translate(&mut carried.clone(), &fx!("Stop"))
    );
}

#[test]
fn translation_state_persists_through_serde() {
    let mut st = state();
    run(
        &mut st,
        &[
            fx!("UserPromptSubmit"),
            fx!("PreToolUse.Bash"),
            fx!("PreCompact"),
        ],
    );
    let json = serde_json::to_value(&st).unwrap();
    let mut restored: ClaudeTranslation = serde_json::from_value(json).unwrap();
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
    // A new translator for an existing session (e.g. after a host restart) must not reuse the old one's ids.
    let session = [
        fx!("UserPromptSubmit"),
        fx!("PermissionRequest.Bash"),
        fx!("PreCompact"),
        fx!("PreCompact"),
        fx!("Stop"),
        fx!("PreToolUse.Bash"),
    ];
    let ids = |seed: &str| -> Vec<String> {
        run(&mut seeded(seed), &session)
            .iter()
            .filter_map(|d| {
                let raw = d.frame.raw();
                let compaction = raw["params"]["update"]["compactionId"].as_str();
                match (d.frame.method(), d.frame.id()) {
                    (Some(_), Some(id)) => Some(id.to_string()),
                    _ => compaction.map(str::to_owned),
                }
            })
            .collect()
    };
    let (a, b) = (ids("e01"), ids("e02"));
    assert_eq!(a.len(), 6);
    for id in &a {
        assert!(!b.contains(id), "{id} repeats across translators");
    }
}

// ---------- tools ----------

#[test]
fn pre_tool_use_becomes_an_in_progress_tool_call_keyed_by_tool_use_id() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Bash")]);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].dir, Dir::FromAgent);
    let SessionUpdate::ToolCall(call) = update(&drafts[0]) else {
        panic!("expected a tool_call");
    };
    assert_eq!(call.tool_call_id, "toolu_01ABC123");
    assert_eq!(call.status, ToolCallStatus::InProgress);
    assert_eq!(call.title, "Bash: npm test");
    assert_eq!(call.kind, ToolKind::Execute);
    assert_eq!(call.raw_input.as_ref().unwrap()["command"], "npm test");
}

#[test]
fn tool_titles_and_kinds_follow_the_tool() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Read")]);
    let SessionUpdate::ToolCall(call) = update(&drafts[0]) else {
        panic!("expected a tool_call");
    };
    assert_eq!(call.title, "Read: /Users/dev/app/src/math.ts");
    assert_eq!(call.kind, ToolKind::Read);
}

#[test]
fn post_tool_use_becomes_a_completed_tool_call_update() {
    let drafts = last(&[fx!("PreToolUse.Bash"), fx!("PostToolUse.Bash")]);
    assert_eq!(drafts.len(), 1);
    let SessionUpdate::ToolCallUpdate(u) = update(&drafts[0]) else {
        panic!("expected a tool_call_update");
    };
    assert_eq!(u.tool_call_id, "toolu_01ABC123");
    assert_eq!(u.status, Some(ToolCallStatus::Completed));
}

#[test]
fn post_tool_use_failure_becomes_a_failed_tool_call_update() {
    let drafts = last(&[fx!("PreToolUse.Bash"), fx!("PostToolUseFailure.Bash")]);
    let SessionUpdate::ToolCallUpdate(u) = update(&drafts[0]) else {
        panic!("expected a tool_call_update");
    };
    assert_eq!(u.tool_call_id, "toolu_01ABC123");
    assert_eq!(u.status, Some(ToolCallStatus::Failed));
}

#[test]
fn permission_denied_fails_the_tool_call() {
    let drafts = last(&[fx!("PreToolUse.Bash"), fx!("PermissionDenied.Bash")]);
    let SessionUpdate::ToolCallUpdate(u) = update(&drafts[0]) else {
        panic!("expected a tool_call_update");
    };
    assert_eq!(u.tool_call_id, "toolu_01ABC123");
    assert_eq!(u.status, Some(ToolCallStatus::Failed));
}

// ---------- permissions ----------

#[test]
fn permission_request_becomes_request_permission_naming_the_pending_tool() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
    ]);
    assert_eq!(
        methods(&drafts),
        vec![
            Some("session/request_permission"),
            Some("_shepherd/agent/asked")
        ]
    );
    assert_eq!(drafts[0].dir, Dir::FromAgent);
    let AcpFrame::RequestPermission(r) = acp(&drafts[0]) else {
        panic!("expected request_permission");
    };
    assert_eq!(r.tool_call.tool_call_id, "toolu_01ABC123");
    assert_eq!(r.tool_call.title.as_deref(), Some("Bash: npm test"));
}

#[test]
fn permission_request_for_an_unseen_tool_gets_its_own_tool_call_id() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("PermissionRequest.Bash")]);
    let AcpFrame::RequestPermission(r) = acp(&drafts[0]) else {
        panic!("expected request_permission");
    };
    assert_ne!(r.tool_call.tool_call_id, "toolu_01ABC123");
    assert!(!r.tool_call.tool_call_id.is_empty());
}

#[test]
fn permission_notification_becomes_request_permission() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("Notification.permission_prompt"),
    ]);
    assert_eq!(
        methods(&drafts),
        vec![
            Some("session/request_permission"),
            Some("_shepherd/agent/asked")
        ]
    );
    assert_eq!(drafts[0].dir, Dir::FromAgent);
    let AcpFrame::RequestPermission(r) = acp(&drafts[0]) else {
        panic!("expected request_permission");
    };
    assert_eq!(
        r.tool_call.title.as_deref(),
        Some("Claude needs your permission to use Bash")
    );
}

#[test]
fn permission_notification_for_an_already_requested_permission_is_not_a_second_request() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
        fx!("Notification.permission_prompt"),
    ]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/Notification")]);
}

#[test]
fn generic_notifications_are_not_permission_requests() {
    let drafts = one(&[fx!("Notification.auth_success")]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/Notification")]);
    assert_eq!(
        drafts[0].frame.raw()["params"],
        fx!("Notification.auth_success")
    );
}

#[test]
fn request_ids_never_repeat_within_a_direction() {
    let drafts = one(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("Notification.permission_prompt"),
        fx!("Stop"),
        fx!("UserPromptSubmit.second"),
        fx!("PermissionRequest.Bash"),
    ]);
    let mut seen = std::collections::HashSet::new();
    for d in drafts.iter().filter(|d| d.frame.method().is_some()) {
        if let Some(id) = d.frame.id() {
            assert!(seen.insert((d.dir.as_str().to_owned(), id.to_string())));
        }
    }
    assert_eq!(seen.len(), 5);
}

// ---------- agent lifecycle ----------

#[test]
fn idle_notification_becomes_agent_idle() {
    let drafts = one(&[fx!("Notification.idle_prompt")]);
    assert_eq!(methods(&drafts), vec![Some("_shepherd/agent/idle")]);
}

#[test]
fn session_start_becomes_agent_started_with_the_agent_session_id() {
    let drafts = one(&[fx!("SessionStart.startup")]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(s.agent, "claude-code");
    assert_eq!(s.agent_session_id.as_deref(), Some("abc123"));
}

#[test]
fn session_start_carries_the_transcript_path_from_the_payload() {
    let drafts = one(&[fx!("SessionStart.startup")]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(
        s.transcript_path.as_deref(),
        Some("/Users/dev/.claude/projects/-Users-dev-app/abc123.jsonl")
    );
}

#[test]
fn session_start_without_a_transcript_path_has_none() {
    let mut payload = fx!("SessionStart.startup");
    payload.as_object_mut().map(|o| o.remove("transcript_path"));
    let drafts = one(&[payload]);
    let ShepherdFrame::AgentStarted(s) = shepherd(&drafts[0]) else {
        panic!("expected agent/started");
    };
    assert_eq!(s.transcript_path, None);
}

#[test]
fn session_start_after_compaction_does_not_restart_the_agent() {
    let drafts = one(&[fx!("SessionStart.compact")]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/SessionStart")]);
}

#[test]
fn session_end_becomes_agent_exited() {
    let drafts = one(&[fx!("SessionEnd.other")]);
    assert!(matches!(
        shepherd(&drafts[0]),
        ShepherdFrame::AgentExited(_)
    ));
}

#[test]
fn session_end_for_clear_is_not_an_exit() {
    let drafts = one(&[fx!("SessionEnd.clear")]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/SessionEnd")]);
}

#[test]
fn stop_failure_ends_the_turn_with_idle_and_keeps_the_payload() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("StopFailure")]);
    assert_eq!(
        methods(&drafts),
        vec![
            Some("_claude-code/StopFailure"),
            Some("_shepherd/agent/idle")
        ]
    );
    assert_eq!(drafts[0].frame.raw()["params"], fx!("StopFailure"));
}

// ---------- compaction ----------

#[test]
fn pre_compact_becomes_an_in_progress_compaction_update() {
    let drafts = one(&[fx!("PreCompact")]);
    let SessionUpdate::CompactionUpdate(c) = update(&drafts[0]) else {
        panic!("expected compaction_update");
    };
    assert_eq!(c.status.as_str(), "in_progress");
    assert!(!c.compaction_id.is_empty());
}

#[test]
fn post_compact_completes_the_same_compaction_with_its_summary() {
    let drafts = one(&[fx!("PreCompact"), fx!("PostCompact")]);
    let (SessionUpdate::CompactionUpdate(start), SessionUpdate::CompactionUpdate(end)) =
        (update(&drafts[0]), update(&drafts[1]))
    else {
        panic!("expected two compaction_updates");
    };
    assert_eq!(start.compaction_id, end.compaction_id);
    assert_eq!(end.status.as_str(), "completed");
    assert_eq!(
        end.summary,
        Some(json!([{ "type": "text", "text": "Summary of the compacted conversation..." }]))
    );
}

#[test]
fn a_new_pre_compact_cancels_a_compaction_that_never_completed() {
    // A hook can block PreCompact, and then no PostCompact follows.
    let mut st = state();
    let first = translate(&mut st, &fx!("PreCompact"));
    let second = translate(&mut st, &fx!("PreCompact"));
    let SessionUpdate::CompactionUpdate(blocked) = update(&first[0]) else {
        panic!("expected compaction_update");
    };
    let updates: Vec<_> = second
        .iter()
        .map(|d| match update(d) {
            SessionUpdate::CompactionUpdate(c) => {
                (c.compaction_id.clone(), c.status.as_str().to_owned())
            }
            other => panic!("expected compaction_update, got {other:?}"),
        })
        .collect();
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[0],
        (blocked.compaction_id.clone(), "cancelled".to_owned())
    );
    assert_ne!(updates[1].0, blocked.compaction_id);
    assert_eq!(updates[1].1, "in_progress");

    let done = translate(&mut st, &fx!("PostCompact"));
    let SessionUpdate::CompactionUpdate(c) = update(&done[0]) else {
        panic!("expected compaction_update");
    };
    assert_eq!(c.compaction_id, updates[1].0);
}

#[test]
fn post_compact_without_a_pre_compact_is_a_vendor_frame() {
    let drafts = one(&[fx!("PostCompact")]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/PostCompact")]);
}

// ---------- other and malformed hooks ----------

#[test]
fn other_hooks_become_vendor_frames_carrying_the_raw_payload() {
    let drafts = one(&[fx!("CwdChanged")]);
    assert_eq!(methods(&drafts), vec![Some("_claude-code/CwdChanged")]);
    assert_eq!(drafts[0].frame.raw()["params"], fx!("CwdChanged"));
    assert!(drafts[0].frame.id().is_none());
}

#[test]
fn payloads_without_a_hook_name_become_unknown_vendor_frames() {
    for payload in [
        json!("not an object"),
        json!(null),
        json!([1, 2]),
        json!({ "session_id": "abc123" }),
        json!({ "hook_event_name": 7 }),
        json!({ "hook_event_name": "has spaces/and slashes" }),
    ] {
        let drafts = one(std::slice::from_ref(&payload));
        assert_eq!(
            methods(&drafts),
            vec![Some("_claude-code/unknown")],
            "{payload}"
        );
        let params = &drafts[0].frame.raw()["params"];
        let carried = if payload.is_object() {
            params
        } else {
            &params["payload"]
        };
        assert_eq!(carried, &payload);
    }
}

#[test]
fn known_hooks_missing_required_fields_become_vendor_frames() {
    for payload in [
        json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" }),
        json!({ "hook_event_name": "PostToolUse", "tool_use_id": 5 }),
        json!({ "hook_event_name": "PostToolUseFailure" }),
    ] {
        let name = payload["hook_event_name"].as_str().unwrap().to_owned();
        let drafts = one(std::slice::from_ref(&payload));
        assert_eq!(
            methods(&drafts),
            vec![Some(format!("_claude-code/{name}").as_str())]
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
        fx!("Notification.permission_prompt"),
        fx!("PostToolUse.Bash"),
        fx!("PreToolUse.Read"),
        fx!("PostToolUseFailure.Bash"),
        fx!("Stop"),
        fx!("Notification.idle_prompt"),
        fx!("SessionEnd.other"),
    ]);
    let reading = Activity::Working {
        tool: Some(ToolSummary {
            id: "toolu_02DEF456".to_owned(),
            title: "Read: /Users/dev/app/src/math.ts".to_owned(),
            kind: ToolKind::Read,
        }),
    };
    assert_eq!(
        seen,
        vec![
            Activity::Starting,
            working(),
            working_on_bash(),
            needs_permission("Bash: npm test"),
            needs_permission("Bash: npm test"),
            working(),
            reading.clone(),
            reading,
            Activity::Idle,
            Activity::Idle,
            Activity::Exited { code: None },
        ]
    );
}

// ---------- turns a Stop hook continues ----------

#[test]
fn a_turn_continued_after_stop_works_again_until_the_next_stop() {
    // Another Stop hook (or /goal) blocked the first Stop, so Claude keeps going and fires Stop again.
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
            working_on_bash(),
            working(),
            Activity::Idle,
        ]
    );
}

#[test]
fn a_continued_turn_is_still_one_user_turn() {
    let mut st = state();
    let mut log = EnvelopeBuilder::new();
    let mut session = SessionState::default();
    let payloads = [
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("Stop.continued"),
    ];
    let user_text = payloads[0]["prompt"].as_str().unwrap().to_owned();
    for d in run(&mut st, &payloads) {
        session.apply(&log.envelope(d.dir.clone(), d.frame.raw().clone()));
    }
    assert_eq!(session.turns(), 1);
    assert_eq!(session.last_prompt(), Some(user_text.as_str()));
}

#[test]
fn a_main_agent_tool_with_no_open_prompt_opens_a_marked_continuation() {
    let drafts = last(&[fx!("UserPromptSubmit"), fx!("Stop"), fx!("PreToolUse.Bash")]);
    assert_eq!(
        methods(&drafts),
        vec![Some("session/prompt"), Some("session/update")]
    );
    let d = &drafts[0];
    assert!(is_continuation(d));
    assert!(d.frame.is_synthesized());
    assert_eq!(d.dir, Dir::ToAgent);
    let AcpFrame::Prompt(p) = acp(d) else {
        panic!("expected a prompt request");
    };
    assert!(p.prompt.is_empty());
}

#[test]
fn the_continued_stop_answers_the_continuation() {
    let drafts = one(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Bash"),
        fx!("Stop.continued"),
    ]);
    let continuation = drafts.iter().find(|d| is_continuation(d)).unwrap();
    let response = drafts.last().unwrap();
    assert!(matches!(response.frame, Frame::Response(_)));
    assert_eq!(response.frame.id(), continuation.frame.id());
    assert_eq!(response.dir, Dir::FromAgent);
}

#[test]
fn a_permission_request_with_no_open_prompt_opens_a_continuation() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PermissionRequest.Bash"),
    ]);
    assert_eq!(
        methods(&drafts),
        vec![
            Some("session/prompt"),
            Some("session/request_permission"),
            Some("_shepherd/agent/asked")
        ]
    );
    assert!(is_continuation(&drafts[0]));
}

#[test]
fn tools_inside_an_open_prompt_open_no_continuation() {
    let drafts = one(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionRequest.Bash"),
        fx!("PostToolUse.Bash"),
        fx!("PreToolUse.Read"),
    ]);
    assert!(!drafts.iter().any(is_continuation));
}

#[test]
fn subagent_tool_hooks_open_no_continuation() {
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Grep.subagent"),
    ]);
    assert!(!drafts.iter().any(is_continuation));
    let drafts = last(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PermissionRequest.Grep.subagent"),
    ]);
    assert!(!drafts.iter().any(is_continuation));

    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("PreToolUse.Grep.subagent"),
    ]);
    assert_eq!(seen.last(), Some(&Activity::Idle));
}

#[test]
fn subagent_tools_during_the_main_turn_are_tracked_as_usual() {
    let seen = activities(&[fx!("UserPromptSubmit"), fx!("PreToolUse.Grep.subagent")]);
    assert!(
        matches!(seen.last(), Some(Activity::Working { tool: Some(t) }) if t.id == "toolu_03GHI789")
    );
}

#[test]
fn the_stop_response_correlates_because_it_travels_opposite_to_the_prompt() {
    let drafts = one(&[fx!("UserPromptSubmit"), fx!("Stop")]);
    let (_, seen) = fold(&drafts);
    assert_eq!(seen.last(), Some(&Activity::Idle));

    // Control: the same response sent in the prompt's own direction doesn't correlate, so the turn never ends.
    let mut flipped = drafts.clone();
    flipped.last_mut().unwrap().dir = Dir::ToAgent;
    let (_, seen) = fold(&flipped);
    assert_eq!(seen.last(), Some(&working()));
}

#[test]
fn an_interrupted_turn_goes_idle_on_the_idle_notification() {
    // Claude Code fires no Stop when the user presses Esc. The idle_prompt notification is the only hook that
    // follows an interrupt if the user doesn't type again.
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("Notification.idle_prompt"),
    ]);
    assert_eq!(seen.last(), Some(&Activity::Idle));
}

#[test]
fn an_interrupted_turn_is_replaced_by_the_next_prompt_and_that_prompt_s_stop_ends_it() {
    let mut st = state();
    let drafts = run(
        &mut st,
        &[
            fx!("UserPromptSubmit"),
            fx!("PreToolUse.Bash"),
            fx!("UserPromptSubmit.second"),
        ],
    );
    let (session, seen) = fold(&drafts);
    assert_eq!(seen.last(), Some(&working()));
    assert_eq!(session.turns(), 2);
    assert!(session.tools().is_empty());

    let mut all = drafts;
    all.extend(translate(&mut st, &fx!("Stop")));
    let (_, seen) = fold(&all);
    assert_eq!(seen.last(), Some(&Activity::Idle));
}

#[test]
fn a_late_stop_after_an_interrupt_and_idle_does_not_answer_a_stale_prompt() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Notification.idle_prompt"),
        fx!("UserPromptSubmit.second"),
        fx!("Stop"),
    ]);
    assert_eq!(
        seen,
        vec![working(), Activity::Idle, working(), Activity::Idle]
    );
}

#[test]
fn a_permission_notification_while_idle_needs_permission() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("Notification.permission_prompt"),
    ]);
    assert_eq!(
        seen.last(),
        Some(&needs_permission(
            "Claude needs your permission to use Bash"
        ))
    );
}

#[test]
fn a_generic_notification_while_idle_stays_idle() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Stop"),
        fx!("Notification.auth_success"),
    ]);
    assert_eq!(seen.last(), Some(&Activity::Idle));
}

#[test]
fn a_hook_permission_is_cleared_by_the_next_tool_activity() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("Notification.permission_prompt"),
        fx!("PreToolUse.Read"),
    ]);
    assert!(
        matches!(seen.last(), Some(Activity::Working { tool: Some(t) }) if t.id == "toolu_02DEF456")
    );
}

#[test]
fn an_api_error_ends_the_turn() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("StopFailure"),
    ]);
    assert_eq!(seen.last(), Some(&Activity::Idle));
}

#[test]
fn a_denied_tool_is_no_longer_in_flight() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PermissionDenied.Bash"),
    ]);
    assert_eq!(seen.last(), Some(&working()));
}

#[test]
fn compaction_mid_turn_keeps_the_session_working() {
    let seen = activities(&[
        fx!("UserPromptSubmit"),
        fx!("PreToolUse.Bash"),
        fx!("PreCompact"),
        fx!("SessionStart.compact"),
        fx!("PostCompact"),
    ]);
    assert_eq!(seen.last(), Some(&working_on_bash()));
}

#[test]
fn clear_restarts_the_agent_under_its_new_session_id() {
    let mut st = state();
    let drafts = run(
        &mut st,
        &[
            fx!("UserPromptSubmit"),
            fx!("Stop"),
            fx!("SessionEnd.clear"),
            fx!("SessionStart.clear"),
            fx!("UserPromptSubmit"),
        ],
    );
    let (_, seen) = fold(&drafts);
    assert_eq!(seen.last(), Some(&working()));
    let started = drafts
        .iter()
        .find_map(|d| match &d.frame {
            Frame::Shepherd(m) => match m.body() {
                ShepherdFrame::AgentStarted(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(started.agent_session_id.as_deref(), Some("def456"));
}

// ---------- properties ----------

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
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

const HOOKS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Notification",
    "Stop",
    "StopFailure",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
    "SubagentStop",
];

/// A payload shaped like a hook: a known hook name, then a mix of plausible and arbitrary field values.
fn arb_hookish() -> impl Strategy<Value = Value> {
    let field = |name: &'static str, plausible: Vec<Value>| {
        (
            Just(name),
            prop_oneof![
                prop::sample::select(plausible).prop_map(Some),
                arb_json().prop_map(Some),
                Just(None),
            ],
        )
    };
    (
        prop::sample::select(HOOKS),
        field("tool_use_id", vec![json!("toolu_1"), json!("toolu_2")]),
        field("tool_name", vec![json!("Bash"), json!("Read")]),
        field("tool_input", vec![json!({ "command": "ls" }), json!({})]),
        field(
            "notification_type",
            vec![
                json!("permission_prompt"),
                json!("idle_prompt"),
                json!("auth_success"),
            ],
        ),
        field(
            "source",
            vec![json!("startup"), json!("compact"), json!("clear")],
        ),
        field("reason", vec![json!("other"), json!("clear")]),
        field("prompt", vec![json!("hi")]),
        field("session_id", vec![json!("abc123")]),
    )
        .prop_map(|(hook, a, b, c, d, e, f, g, h)| {
            let mut obj = serde_json::Map::new();
            obj.insert("hook_event_name".to_owned(), json!(hook));
            for (k, v) in [a, b, c, d, e, f, g, h] {
                if let Some(v) = v {
                    obj.insert(k.to_owned(), v);
                }
            }
            Value::Object(obj)
        })
}

proptest! {
    #[test]
    fn translating_and_folding_any_payloads_never_panics(
        payloads in prop::collection::vec(prop_oneof![arb_json(), arb_hookish()], 0..32)
    ) {
        let mut st = state();
        let mut log = EnvelopeBuilder::new();
        let mut session = SessionState::default();
        for p in &payloads {
            for d in translate(&mut st, p) {
                // Every draft is a well-formed frame that parses back to itself.
                prop_assert_eq!(&Frame::from_value(d.frame.raw().clone()).unwrap(), &d.frame);
                session.apply(&log.envelope(d.dir.clone(), d.frame.raw().clone()));
            }
        }
    }

    #[test]
    fn every_payload_produces_at_least_one_frame(payload in prop_oneof![arb_json(), arb_hookish()]) {
        prop_assert!(!translate(&mut state(), &payload).is_empty());
    }
}
