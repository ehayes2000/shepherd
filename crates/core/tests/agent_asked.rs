//! What an agent is blocked on asking the user (T51): Claude Code's and Codex's permission and question hooks become
//! `_shepherd/agent/asked`, and the session fold keeps the pending question until it is answered
//! (`_shepherd/session/answered`, or the agent's own activity) or the turn moves on. Checked against the fixtures in
//! `tests/fixtures/{claude,codex}/`.

use serde_json::{Value, json};
use shepherd_core::acpp::{
    AgentAsked, BlockKind, Choice, FrameDraft, PromptBy, SessionAnswered, ShepherdFrame,
};
use shepherd_core::agents::Translators;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::session::{Activity, SessionState, SubagentStatus};
use shepherd_testkit::builders::EnvelopeBuilder;

macro_rules! cc {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/claude/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

macro_rules! cx {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("fixtures/codex/", $name, ".json")))
            .expect(concat!("fixture ", $name, " is valid JSON"))
    };
}

/// Translates one agent's hooks in one session and folds every frame, as the host appends them.
struct Run {
    agent: AgentKind,
    translators: Translators,
    log: EnvelopeBuilder,
    state: SessionState,
    last: Vec<FrameDraft>,
}

impl Run {
    fn new(agent: AgentKind) -> Self {
        Self {
            agent,
            translators: Translators::new(),
            log: EnvelopeBuilder::new(),
            state: SessionState::default(),
            last: Vec::new(),
        }
    }

    fn hook(&mut self, payload: Value) -> &mut Self {
        let session = SessionId::new("s_ask");
        self.last = self
            .translators
            .translate(&session, self.agent, &payload, || "seed".to_owned());
        for d in &self.last {
            let env = self.log.envelope(d.dir.clone(), d.frame.raw().clone());
            self.state.apply(&env);
        }
        self
    }

    fn hooks(&mut self, payloads: Vec<Value>) -> &mut Self {
        for p in payloads {
            self.hook(p);
        }
        self
    }

    /// Folds a frame the host appends.
    fn host(&mut self, frame: ShepherdFrame) -> &mut Self {
        let env = self.log.internal(frame.to_frame().raw().clone());
        self.state.apply(&env);
        self
    }

    /// The `agent/asked` frames the latest hook gave.
    fn asked(&self) -> Vec<AgentAsked> {
        self.last
            .iter()
            .filter_map(|d| match &d.frame {
                shepherd_core::acpp::Frame::Shepherd(m) => match m.body() {
                    ShepherdFrame::AgentAsked(a) => Some(a.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The tool call the latest hook's `session/request_permission` names.
    fn requested_tool(&self) -> Option<String> {
        self.last
            .iter()
            .find(|d| d.frame.method() == Some("session/request_permission"))
            .and_then(|d| d.frame.raw()["params"]["toolCall"]["toolCallId"].as_str())
            .map(str::to_owned)
    }

    fn question(&self) -> Option<&AgentAsked> {
        self.state.question()
    }
}

fn permission(text: &str, tool: Option<&str>) -> AgentAsked {
    AgentAsked {
        kind: BlockKind::Permission,
        text: text.to_owned(),
        choices: None,
        subagent: None,
        tool: tool.map(str::to_owned),
    }
}

fn framework(subagent: Option<&str>) -> AgentAsked {
    AgentAsked {
        kind: BlockKind::Question,
        text: "Which framework?".to_owned(),
        choices: Some(vec!["React".to_owned(), "Vue".to_owned()]),
        subagent: subagent.map(str::to_owned),
        tool: Some("AskUserQuestion".to_owned()),
    }
}

fn answered(choice: Option<Choice>) -> ShepherdFrame {
    ShepherdFrame::SessionAnswered(SessionAnswered {
        choice,
        by: PromptBy::Cli,
    })
}

fn with(mut payload: Value, key: &str, value: Value) -> Value {
    payload[key] = value;
    payload
}

/// The `PostToolUse` that ends the main agent's `AskUserQuestion` call (the user picked an answer).
fn question_answered() -> Value {
    let post = with(
        cc!("PostToolUse.Bash"),
        "tool_name",
        json!("AskUserQuestion"),
    );
    with(post, "tool_use_id", json!("toolu_08ASK123"))
}

// ---------- Claude Code ----------

#[test]
fn a_claude_permission_request_records_what_it_asks_after_the_request() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("PreToolUse.Bash")]);
    run.hook(cc!("PermissionRequest.Bash"));
    let methods: Vec<_> = run.last.iter().map(|d| d.frame.method()).collect();
    assert_eq!(
        methods,
        [
            Some("session/request_permission"),
            Some("_shepherd/agent/asked")
        ]
    );
    assert_eq!(run.asked(), [permission("Bash: npm test", Some("Bash"))]);
    assert_eq!(
        run.question(),
        Some(&permission("Bash: npm test", Some("Bash")))
    );
    assert!(matches!(
        run.state.activity(),
        Activity::NeedsPermission { .. }
    ));
}

#[test]
fn a_claude_permission_notification_without_a_request_records_what_it_asks() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("Notification.permission_prompt"),
    ]);
    let asked = permission("Claude needs your permission to use Bash", None);
    assert_eq!(run.asked(), std::slice::from_ref(&asked));
    assert_eq!(run.question(), Some(&asked));
}

#[test]
fn a_claude_question_records_its_options_and_waits_on_the_user() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
    ]);
    assert_eq!(run.asked(), [framework(None)]);
    assert_eq!(run.question(), Some(&framework(None)));
    assert_eq!(run.state.activity(), &Activity::NeedsInput);
    assert!(run.state.wants_user());
    assert!(!run.state.at_prompt());
    assert_eq!(run.state.asked(), Some("Which framework?"));

    // The permission notification that follows the dialog isn't a second question.
    run.hook(cc!("Notification.permission_prompt"));
    assert!(run.asked().is_empty());
    assert_eq!(run.question(), Some(&framework(None)));

    // Picking an answer in the pane finishes the tool call: the question is gone.
    run.hook(question_answered());
    assert_eq!(run.question(), None);
    assert!(matches!(run.state.activity(), Activity::Working { .. }));
}

#[test]
fn a_question_that_takes_more_than_one_pick_has_no_choices() {
    let mut ask = cc!("PreToolUse.AskUserQuestion");
    ask["tool_input"]["questions"][0]["multiSelect"] = json!(true);
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![cc!("UserPromptSubmit"), ask]);
    assert_eq!(run.question().map(|q| q.choices.clone()), Some(None));

    let mut two = cc!("PreToolUse.AskUserQuestion");
    let first = two["tool_input"]["questions"][0].clone();
    two["tool_input"]["questions"] = json!([first.clone(), first]);
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![cc!("UserPromptSubmit"), two]);
    assert_eq!(run.question().map(|q| q.choices.clone()), Some(None));
}

#[test]
fn a_subagents_permission_and_question_are_asked_in_the_parent_naming_the_subagent() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    let asked = run.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, BlockKind::Permission);
    assert_eq!(asked[0].subagent.as_deref(), Some("agent-7f3a"));
    assert_eq!(run.question(), asked.first());

    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.AskUserQuestion.subagent"),
    ]);
    assert_eq!(run.question(), Some(&framework(Some("agent-7f3a"))));
}

// ---------- Codex ----------

#[test]
fn a_codex_permission_request_records_what_it_asks() {
    let mut run = Run::new(AgentKind::Codex);
    run.hooks(vec![cx!("UserPromptSubmit"), cx!("PreToolUse.Bash")]);
    run.hook(cx!("PermissionRequest.Bash"));
    assert_eq!(run.asked(), [permission("Bash: cargo test", Some("Bash"))]);
    assert_eq!(
        run.question(),
        Some(&permission("Bash: cargo test", Some("Bash")))
    );

    let mut run = Run::new(AgentKind::Codex);
    run.hooks(vec![
        cx!("UserPromptSubmit"),
        cx!("SubagentStart"),
        cx!("PermissionRequest.Bash.subagent"),
    ]);
    assert_eq!(run.asked().len(), 1);
    assert!(run.question().is_some_and(|q| q.subagent.is_some()));
}

// ---------- the fold ----------

fn asking_claude() -> Run {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
    ]);
    assert!(run.question().is_some());
    run
}

#[test]
fn an_answer_shepherd_typed_clears_the_question_and_the_agent_works_on() {
    let mut run = asking_claude();
    run.host(answered(Some(Choice::Yes)));
    assert_eq!(run.question(), None);
    assert!(matches!(run.state.activity(), Activity::Working { .. }));
    assert!(!run.state.wants_user());
}

#[test]
fn typing_in_the_pane_takes_the_question_from_shepherd_but_the_agent_still_waits() {
    let mut run = asking_claude();
    run.host(answered(None));
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
    assert!(matches!(
        run.state.activity(),
        Activity::NeedsPermission { .. }
    ));
}

#[test]
fn the_question_is_gone_once_the_agent_moves_on_or_the_turn_ends() {
    let mut run = asking_claude();
    run.hook(cc!("PostToolUse.Bash"));
    assert_eq!(run.question(), None);

    let mut run = asking_claude();
    run.hook(cc!("Stop"));
    assert_eq!(run.question(), None);

    let mut run = asking_claude();
    run.hook(cc!("UserPromptSubmit.second"));
    assert_eq!(run.question(), None);

    let mut run = asking_claude();
    run.hook(cc!("SessionEnd.other"));
    assert_eq!(run.question(), None);
}

#[test]
fn answering_a_subagents_question_sets_that_subagent_working() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    run.host(answered(Some(Choice::No)));
    assert_eq!(run.question(), None);
    assert_eq!(run.state.subagents()[0].status(), &SubagentStatus::Working);
    assert!(!run.state.wants_user());
}

#[test]
fn an_asked_frame_with_nothing_waiting_and_no_turn_is_ignored() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.host(ShepherdFrame::AgentAsked(permission("stray", None)));
    assert_eq!(run.question(), None);
    assert!(!run.state.wants_user());
}

#[test]
fn asked_and_answered_round_trip_through_the_log() {
    for frame in [
        ShepherdFrame::AgentAsked(framework(Some("agent-1"))),
        ShepherdFrame::AgentAsked(permission("Bash: ls", Some("Bash"))),
        answered(Some(Choice::Always)),
        answered(Some(Choice::Option(std::num::NonZeroU32::MIN))),
        answered(None),
    ] {
        let raw = frame.to_frame();
        let parsed = shepherd_core::acpp::Frame::from_value(raw.raw().clone())
            .unwrap_or_else(|e| panic!("{e}"));
        match parsed {
            shepherd_core::acpp::Frame::Shepherd(m) => assert_eq!(m.body(), &frame),
            other => panic!("not a shepherd frame: {other:?}"),
        }
    }
    assert_eq!(
        answered(Some(Choice::Option(
            std::num::NonZeroU32::MIN.saturating_add(1)
        )))
        .to_frame()
        .raw()["params"],
        json!({ "choice": 2, "by": "cli" })
    );
    assert_eq!(
        answered(None).to_frame().raw()["params"],
        json!({ "by": "cli" })
    );
}

#[test]
fn the_question_is_named_by_its_asked_entrys_seq() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("PreToolUse.Bash")]);
    run.hook(cc!("PermissionRequest.Bash"));
    // The hook's frames are the request, then `agent/asked`: the log's last entry.
    let last = run.log.next_seq().get() - 1;
    assert_eq!(run.state.question_seq().map(|s| s.get()), Some(last));
    run.host(answered(Some(Choice::Yes)));
    assert_eq!(run.state.question_seq(), None);
}

/// Claude Code queues dialogs and keeps showing the first: a second one while one is up makes the session
/// unanswerable (it still wants the user) until the agent's activity shows each closed.
#[test]
fn a_dialog_opened_while_another_is_up_leaves_nothing_answerable_until_both_close() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
    ]);
    assert!(run.question().is_some());
    // The user moves around the Bash dialog; then the subagent asks too.
    run.host(answered(None));
    run.hooks(vec![
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());

    // Without the user's typing, the second dialog alone does it.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
    // The subagent's tool runs: its dialog closed, but the Bash one may still be up.
    run.hook(cc!("PostToolUse.Grep.subagent"));
    assert_eq!(run.question(), None);
    // Bash's own activity shows that one closed too; the next dialog is answerable again.
    run.hooks(vec![cc!("PostToolUse.Bash"), cc!("PreToolUse.Read")]);
    run.hook(with(
        cc!("PermissionRequest.Bash"),
        "tool_input",
        json!({ "file_path": "/Users/dev/app/src/math.ts" }),
    ));
    assert!(run.question().is_some());
}

#[test]
fn a_question_while_a_permission_is_up_is_queued_behind_it() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
        cc!("PreToolUse.AskUserQuestion.subagent"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn a_stray_question_outside_a_turn_is_ignored() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("Stop")]);
    run.host(ShepherdFrame::AgentAsked(framework(None)));
    assert_eq!(run.question(), None);
    assert_eq!(run.state.activity(), &Activity::Idle);
    assert!(!run.state.wants_user());
}

/// The main agent's `PermissionRequest` for the question its `AskUserQuestion` call asks.
fn question_request() -> Value {
    let ask = cc!("PreToolUse.AskUserQuestion");
    let request = with(
        cc!("PermissionRequest.Bash"),
        "tool_name",
        json!("AskUserQuestion"),
    );
    with(request, "tool_input", ask["tool_input"].clone())
}

#[test]
fn typing_into_a_dialog_carries_over_to_the_one_queued_behind_it() {
    // The user moved around the Bash dialog; once Bash's activity shows that one closed, the subagent's dialog is
    // what the user's keys went to, as far as shepherd can tell: still not shepherd's to answer.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
    ]);
    run.host(answered(None));
    run.hooks(vec![
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
        cc!("PostToolUse.Bash"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn a_request_for_the_tool_whose_dialog_is_up_is_that_dialog() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
        question_request(),
    ]);
    assert_eq!(run.question(), Some(&framework(None)));

    // Typed into before its request arrived: still the user's.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
    ]);
    run.host(answered(None));
    run.hook(question_request());
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn the_same_question_asked_again_is_still_answerable() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
    ]);
    run.host(ShepherdFrame::AgentAsked(framework(None)));
    assert_eq!(run.question(), Some(&framework(None)));
}

#[test]
fn the_turn_ending_shows_every_queued_dialog_closed() {
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    assert_eq!(run.question(), None);
    // The next turn asks about a tool neither queued dialog was for: nothing of theirs shows them closed but the
    // turn's end.
    let read = with(cc!("PermissionRequest.Bash"), "tool_name", json!("Read"));
    let read = with(
        read,
        "tool_input",
        cc!("PreToolUse.Read")["tool_input"].clone(),
    );
    run.hooks(vec![
        cc!("Stop"),
        cc!("UserPromptSubmit.second"),
        cc!("PreToolUse.Read"),
        read,
    ]);
    assert!(run.question().is_some());
}

#[test]
fn a_dialog_on_no_known_tool_is_never_shown_closed_by_other_tools() {
    // The parent's Bash request names no tool it started; the subagent's Grep dialog queues behind it. A tool
    // unrelated to either running proves neither closed.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Bash"),
        cc!("PermissionRequest.Grep.subagent"),
        cc!("PreToolUse.Read"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn a_dialog_on_no_known_tool_cleared_by_other_activity_may_still_be_up() {
    // A Bash request on no known tool; a subagent's tool runs (which drops it, though it may still be on screen),
    // then the subagent asks: not answerable until the turn ends.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PermissionRequest.Bash"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
    run.hooks(vec![
        cc!("Stop"),
        cc!("UserPromptSubmit.second"),
        cc!("PreToolUse.Bash"),
        cc!("PermissionRequest.Bash"),
    ]);
    assert!(run.question().is_some());
}

/// `payload` as a hook from subagent `agent-7f3a` (an `Explore`), its tool call `tool_use_id`.
fn from_subagent(payload: Value, tool_use_id: &str) -> Value {
    let payload = with(payload, "agent_id", json!("agent-7f3a"));
    let payload = with(payload, "agent_type", json!("Explore"));
    match payload.get("tool_use_id") {
        Some(_) => with(payload, "tool_use_id", json!(tool_use_id)),
        None => payload,
    }
}

#[test]
fn the_agent_and_a_subagent_asking_for_the_same_command_are_two_dialogs() {
    // Both run `npm test`. A `PermissionRequest` names no tool call: each is linked to its own agent's call, so the
    // subagent's request is queued behind the agent's dialog, which Claude Code keeps in front.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Bash"),
        from_subagent(cc!("PreToolUse.Bash"), "toolu_09SUB456"),
    ]);
    run.hook(cc!("PermissionRequest.Bash"));
    assert_eq!(run.requested_tool().as_deref(), Some("toolu_01ABC123"));
    run.hook(from_subagent(cc!("PermissionRequest.Bash"), ""));
    assert_eq!(run.requested_tool().as_deref(), Some("toolu_09SUB456"));
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn a_codex_subagents_request_is_linked_to_its_own_call() {
    // The main thread and a subagent run the same command; the main thread's request comes first.
    let sub = |p: Value| {
        let p = with(p, "agent_id", json!("agent-7f3a"));
        with(p, "agent_type", json!("explorer"))
    };
    let mut run = Run::new(AgentKind::Codex);
    run.hooks(vec![
        cx!("UserPromptSubmit"),
        cx!("SubagentStart"),
        cx!("PreToolUse.Bash"),
        with(
            sub(cx!("PreToolUse.Bash")),
            "tool_use_id",
            json!("call_sub_same"),
        ),
    ]);
    run.hook(cx!("PermissionRequest.Bash"));
    assert_eq!(run.requested_tool().as_deref(), Some("call_bash_1"));
    run.hook(sub(cx!("PermissionRequest.Bash")));
    assert_eq!(run.requested_tool().as_deref(), Some("call_sub_same"));
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}

#[test]
fn a_request_taken_for_the_dialog_up_but_asked_by_another_agent_is_not_answerable() {
    // Should a request still be linked to the tool whose dialog is up, an ask from a different (sub)agent shows it is
    // another dialog.
    let mut run = Run::new(AgentKind::ClaudeCode);
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.AskUserQuestion"),
    ]);
    let request = json!({ "jsonrpc": "2.0", "id": "p-other", "method": "session/request_permission",
                          "params": { "sessionId": "abc123",
                                      "toolCall": { "toolCallId": "toolu_08ASK123", "title": "Bash: npm test" },
                                      "options": [] } });
    let env = run.log.from_agent(request);
    run.state.apply(&env);
    let mut theirs = permission("Bash: npm test", Some("Bash"));
    theirs.subagent = Some("agent-7f3a".to_owned());
    run.host(ShepherdFrame::AgentAsked(theirs));
    assert_eq!(run.question(), None);
    assert!(run.state.wants_user());
}
