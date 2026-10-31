//! Subagent lifecycle: Claude Code and Codex subagent hooks become `_shepherd/subagent/*` frames
//! (`shepherd_core::agents`), and the session fold (`shepherd_core::session`) carries the live subagents and lets a
//! blocked one want the user. Checked against the fixtures in `tests/fixtures/{claude,codex}/`.

use serde_json::{Value, json};
use shepherd_core::acpp::{
    BlockKind, Dir, Frame, FrameDraft, Seq, ShepherdFrame, SubagentBlocked, SubagentFinished,
    SubagentStarted, SubagentWorking,
};
use shepherd_core::agents::{Translators, claude, codex};
use shepherd_core::config::AgentKind;
use shepherd_core::display::display_safe;
use shepherd_core::ids::SessionId;
use shepherd_core::session::{Activity, SessionState, Subagent, SubagentStatus};
use shepherd_testkit::builders::EnvelopeBuilder;

// ---------- fixtures ----------

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

// ---------- helpers ----------

/// Translates hook payloads from one agent in one session and folds every frame into that session's state, the way
/// the host appends them and a client folds the log.
struct Run {
    agent: AgentKind,
    translators: Translators,
    log: EnvelopeBuilder,
    state: SessionState,
    /// The drafts of the latest payload.
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

    fn claude() -> Self {
        Self::new(AgentKind::ClaudeCode)
    }

    fn codex() -> Self {
        Self::new(AgentKind::Codex)
    }

    /// Translates and folds `payload`; returns the seq of its last frame.
    fn hook(&mut self, payload: Value) -> Seq {
        let session = SessionId::new("s_sub");
        self.last = self
            .translators
            .translate(&session, self.agent, &payload, || "seed".to_owned());
        let mut seq = Seq::ZERO;
        for d in &self.last {
            let env = self.log.envelope(d.dir.clone(), d.frame.raw().clone());
            seq = env.seq();
            self.state.apply(&env);
        }
        seq
    }

    fn hooks(&mut self, payloads: Vec<Value>) -> Seq {
        let mut seq = Seq::ZERO;
        for p in payloads {
            seq = self.hook(p);
        }
        seq
    }

    /// The `_shepherd/subagent/*` frames the latest payload produced.
    fn subagent_frames(&self) -> Vec<ShepherdFrame> {
        self.last
            .iter()
            .filter_map(|d| match &d.frame {
                Frame::Shepherd(m) if m.body().method().starts_with("_shepherd/subagent/") => {
                    Some(m.body().clone())
                }
                _ => None,
            })
            .collect()
    }

    fn methods(&self) -> Vec<Option<&str>> {
        self.last.iter().map(|d| d.frame.method()).collect()
    }

    fn subagents(&self) -> &[Subagent] {
        self.state.subagents()
    }

    fn only(&self) -> &Subagent {
        match self.subagents() {
            [one] => one,
            other => panic!("expected one subagent, got {other:?}"),
        }
    }
}

fn started(id: &str, agent_type: &str, task: Option<&str>) -> ShepherdFrame {
    ShepherdFrame::SubagentStarted(SubagentStarted {
        id: id.to_owned(),
        agent_type: agent_type.to_owned(),
        task: task.map(str::to_owned),
    })
}

fn blocked(id: &str, agent_type: &str, kind: BlockKind, text: &str) -> ShepherdFrame {
    ShepherdFrame::SubagentBlocked(SubagentBlocked {
        id: id.to_owned(),
        kind,
        text: text.to_owned(),
        agent_type: Some(agent_type.to_owned()),
    })
}

fn working(id: &str) -> ShepherdFrame {
    ShepherdFrame::SubagentWorking(SubagentWorking { id: id.to_owned() })
}

fn finished(id: &str) -> ShepherdFrame {
    ShepherdFrame::SubagentFinished(SubagentFinished { id: id.to_owned() })
}

fn blocked_status(kind: BlockKind, text: &str) -> SubagentStatus {
    SubagentStatus::Blocked {
        kind,
        text: text.to_owned(),
    }
}

// ---------- hooks installed ----------

#[test]
fn both_agents_are_launched_with_their_subagent_hooks() {
    for hook in ["SubagentStart", "SubagentStop"] {
        assert!(claude::HOOK_EVENTS.contains(&hook), "claude: {hook}");
        assert!(codex::HOOK_EVENTS.contains(&hook), "codex: {hook}");
        assert!(codex::hook_trust_key(hook).is_some(), "codex trust: {hook}");
    }
}

// ---------- Claude Code: start ----------

#[test]
fn a_claude_subagent_starts_with_the_task_of_the_agent_tool_that_spawned_it() {
    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("PreToolUse.Agent")]);
    run.hook(cc!("SubagentStart"));
    assert_eq!(
        run.subagent_frames(),
        vec![started(
            "agent-7f3a",
            "Explore",
            Some("Find factorial callers")
        )]
    );
    let sub = run.only();
    assert_eq!(sub.id(), "agent-7f3a");
    assert_eq!(sub.agent_type(), "Explore");
    assert_eq!(sub.task(), Some("Find factorial callers"));
    assert_eq!(sub.status(), &SubagentStatus::Working);
}

#[test]
fn a_claude_subagent_with_no_agent_tool_before_it_starts_without_a_task() {
    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("SubagentStart")]);
    assert_eq!(
        run.subagent_frames(),
        vec![started("agent-7f3a", "Explore", None)]
    );
    assert_eq!(run.only().task(), None);
}

#[test]
fn an_agent_tool_spawning_another_type_lends_its_task_to_no_one() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent.hostile"),
        cc!("SubagentStart"),
    ]);
    assert_eq!(run.only().task(), None);
}

#[test]
fn an_agent_tool_from_an_earlier_turn_lends_its_task_to_no_one() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent"),
        cc!("Stop"),
        cc!("UserPromptSubmit.second"),
        cc!("SubagentStart"),
    ]);
    assert_eq!(run.only().task(), None);
}

#[test]
fn an_agent_tool_that_finished_without_a_subagent_lends_its_task_to_no_one() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent"),
        cc!("PostToolUse.Agent"),
        cc!("SubagentStart"),
    ]);
    assert_eq!(run.only().task(), None);
}

#[test]
fn claude_codes_internal_agents_are_not_subagents() {
    // Prompt suggestions and `/btw` run as agents with an empty `agent_type` (hooks reference, SubagentStop input).
    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit")]);
    run.hook(cc!("SubagentStart.internal"));
    assert_eq!(run.methods(), vec![Some("_claude-code/SubagentStart")]);
    assert_eq!(
        run.last[0].frame.raw()["params"],
        cc!("SubagentStart.internal")
    );
    run.hook(cc!("SubagentStop.internal"));
    assert!(run.subagents().is_empty(), "{:?}", run.subagents());
}

// ---------- Claude Code: block ----------

#[test]
fn a_claude_subagent_asking_permission_is_blocked_on_it() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
    ]);
    run.hook(cc!("PermissionRequest.Grep.subagent"));
    // The permission request itself is still logged, as before.
    assert!(
        run.methods().contains(&Some("session/request_permission")),
        "{:?}",
        run.methods()
    );
    assert_eq!(
        run.subagent_frames(),
        vec![blocked(
            "agent-7f3a",
            "Explore",
            BlockKind::Permission,
            "Grep: factorial"
        )]
    );
    assert_eq!(
        run.only().status(),
        &blocked_status(BlockKind::Permission, "Grep: factorial")
    );
}

#[test]
fn a_claude_subagent_asking_a_question_is_blocked_on_it() {
    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("SubagentStart")]);
    run.hook(cc!("PreToolUse.AskUserQuestion.subagent"));
    assert_eq!(
        run.subagent_frames(),
        vec![blocked(
            "agent-7f3a",
            "Explore",
            BlockKind::Question,
            "Which framework?"
        )]
    );
    assert_eq!(
        run.only().status(),
        &blocked_status(BlockKind::Question, "Which framework?")
    );
}

#[test]
fn a_blocked_claude_subagent_works_again_on_its_next_tool_activity() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    run.hook(cc!("PostToolUse.Grep.subagent"));
    assert_eq!(run.subagent_frames(), vec![working("agent-7f3a")]);
    assert_eq!(run.only().status(), &SubagentStatus::Working);
    // Tool activity of a subagent that isn't blocked says nothing new.
    run.hook(cc!("PreToolUse.Grep.subagent"));
    assert_eq!(run.subagent_frames(), vec![]);
}

#[test]
fn the_same_block_twice_is_logged_once() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    run.hook(cc!("PermissionRequest.Grep.subagent"));
    assert_eq!(run.subagent_frames(), vec![]);
}

#[test]
fn a_block_from_a_subagent_the_session_lost_track_of_brings_it_back() {
    // E.g. a background subagent still running after the parent's turn ended.
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("Stop"),
    ]);
    assert!(run.subagents().is_empty());
    let seq = run.hook(cc!("PermissionRequest.Grep.subagent"));
    let sub = run.only();
    assert_eq!(sub.agent_type(), "Explore");
    assert_eq!(sub.task(), None);
    assert_eq!(
        sub.status(),
        &blocked_status(BlockKind::Permission, "Grep: factorial")
    );
    assert!(run.state.attention(Seq::new(seq.get() - 1)));
}

// ---------- Claude Code: attention ----------

#[test]
fn a_blocked_subagent_wants_the_user_even_while_the_parent_is_working() {
    let mut run = Run::claude();
    let before = run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent"),
        cc!("SubagentStart"),
    ]);
    assert!(!run.state.attention(before));
    let block = run.hook(cc!("PermissionRequest.Grep.subagent"));
    // The parent goes on working: its own tool activity clears the session-level permission, which named no tool in
    // flight.
    run.hook(cc!("PreToolUse.Bash"));
    assert!(
        matches!(run.state.activity(), Activity::Working { .. }),
        "{:?}",
        run.state.activity()
    );
    assert!(run.state.attention(before));
    // A block the user has already seen doesn't want them again.
    assert!(!run.state.attention(block));
}

#[test]
fn a_subagent_working_again_no_longer_wants_the_user() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PreToolUse.Grep.subagent"),
        cc!("PermissionRequest.Grep.subagent"),
        cc!("PostToolUse.Grep.subagent"),
    ]);
    assert!(!run.state.attention(Seq::ZERO));
}

#[test]
fn a_working_subagent_does_not_want_the_user() {
    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit"), cc!("SubagentStart")]);
    assert!(!run.state.attention(Seq::ZERO));
}

// ---------- Claude Code: finish and turn end ----------

#[test]
fn a_finished_claude_subagent_stays_until_the_parents_turn_ends() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("PreToolUse.Agent"),
        cc!("SubagentStart"),
    ]);
    run.hook(cc!("SubagentStop"));
    assert_eq!(run.subagent_frames(), vec![finished("agent-7f3a")]);
    assert_eq!(run.only().status(), &SubagentStatus::Finished);
    assert_eq!(run.only().task(), Some("Find factorial callers"));
    run.hook(cc!("PostToolUse.Agent"));
    assert_eq!(run.subagents().len(), 1);
    run.hook(cc!("Stop"));
    assert!(run.subagents().is_empty(), "{:?}", run.subagents());
}

#[test]
fn a_subagent_that_never_reports_finishing_is_dropped_at_the_parents_turn_end() {
    let mut run = Run::claude();
    run.hooks(vec![
        cc!("UserPromptSubmit"),
        cc!("SubagentStart"),
        cc!("PermissionRequest.Grep.subagent"),
    ]);
    assert_eq!(run.subagents().len(), 1);
    run.hook(cc!("Stop"));
    assert!(run.subagents().is_empty(), "{:?}", run.subagents());
}

#[test]
fn every_way_a_parents_turn_ends_drops_its_subagents() {
    let enders = [
        cc!("Notification.idle_prompt"),
        cc!("StopFailure"),
        cc!("UserPromptSubmit.second"),
        cc!("SessionStart.clear"),
        cc!("SessionEnd.other"),
    ];
    for ender in enders {
        let mut run = Run::claude();
        run.hooks(vec![cc!("UserPromptSubmit"), cc!("SubagentStart")]);
        let name = ender["hook_event_name"].clone();
        run.hook(ender);
        assert!(run.subagents().is_empty(), "{name}: {:?}", run.subagents());
    }
}

// ---------- hostile text ----------

#[test]
fn hostile_agent_type_and_task_are_kept_raw_in_the_log_and_are_display_safe_when_drawn() {
    let spawn = cc!("PreToolUse.Agent.hostile");
    let start = cc!("SubagentStart.hostile");
    let raw_type = start["agent_type"].as_str().unwrap().to_owned();
    let raw_task = spawn["tool_input"]["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(raw_type.chars().any(char::is_control));
    assert!(raw_task.chars().any(char::is_control));

    let mut run = Run::claude();
    run.hooks(vec![cc!("UserPromptSubmit"), spawn]);
    run.hook(start);
    let params = run
        .last
        .iter()
        .find(|d| d.frame.method() == Some("_shepherd/subagent/started"))
        .map(|d| d.frame.raw()["params"].clone())
        .unwrap_or_else(|| panic!("no started frame in {:?}", run.methods()));
    assert_eq!(params["agent_type"], json!(raw_type));
    assert_eq!(params["task"], json!(raw_task));

    let sub = run.only();
    assert_eq!(sub.agent_type(), raw_type);
    assert_eq!(sub.task(), Some(raw_task.as_str()));

    let shown = format!(
        "{} {}",
        display_safe(sub.agent_type(), 80).as_str(),
        display_safe(sub.task().unwrap_or_default(), 80).as_str()
    );
    assert!(
        !shown
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{202e}' | '\u{200b}')),
        "{shown:?}"
    );
    assert!(shown.contains("Evil"), "{shown:?}");
    assert!(shown.contains("login"), "{shown:?}");
}

// ---------- Codex ----------

#[test]
fn a_codex_subagent_starts_with_the_message_of_the_spawn_agent_call() {
    let mut run = Run::codex();
    run.hooks(vec![cx!("UserPromptSubmit"), cx!("PreToolUse.spawn_agent")]);
    run.hook(cx!("SubagentStart"));
    assert_eq!(
        run.subagent_frames(),
        vec![started(
            "agent-7f3a",
            "explorer",
            Some("Find every TODO under src and summarize them.")
        )]
    );
    assert_eq!(run.only().status(), &SubagentStatus::Working);
}

#[test]
fn a_codex_subagent_asking_permission_is_blocked_until_its_tool_finishes() {
    let mut run = Run::codex();
    let before = run.hooks(vec![
        cx!("UserPromptSubmit"),
        cx!("SubagentStart"),
        cx!("PreToolUse.Bash.subagent"),
    ]);
    run.hook(cx!("PermissionRequest.Bash.subagent"));
    assert_eq!(
        run.subagent_frames(),
        vec![blocked(
            "agent-7f3a",
            "explorer",
            BlockKind::Permission,
            "Bash: rg TODO"
        )]
    );
    assert!(run.state.attention(before));
    run.hook(cx!("PostToolUse.Bash.subagent"));
    assert_eq!(run.subagent_frames(), vec![working("agent-7f3a")]);
    assert_eq!(run.only().status(), &SubagentStatus::Working);
}

#[test]
fn a_finished_codex_subagent_stays_until_the_parents_turn_ends() {
    let mut run = Run::codex();
    run.hooks(vec![
        cx!("UserPromptSubmit"),
        cx!("PreToolUse.spawn_agent"),
        cx!("SubagentStart"),
        cx!("PostToolUse.spawn_agent"),
    ]);
    run.hook(cx!("SubagentStop"));
    assert_eq!(run.subagent_frames(), vec![finished("agent-7f3a")]);
    assert_eq!(run.only().status(), &SubagentStatus::Finished);
    run.hook(cx!("Stop"));
    assert!(run.subagents().is_empty());
}

#[test]
fn a_codex_subagent_that_never_reports_finishing_is_dropped_at_the_parents_turn_end() {
    for ender in [cx!("Stop"), cx!("Interrupt")] {
        let mut run = Run::codex();
        run.hooks(vec![cx!("UserPromptSubmit"), cx!("SubagentStart")]);
        run.hook(ender);
        assert!(run.subagents().is_empty());
    }
}

// ---------- the fold, from frames ----------

fn frame(f: &ShepherdFrame) -> Value {
    f.to_frame().raw().clone()
}

fn fold(frames: &[Value]) -> (SessionState, Vec<Seq>) {
    let mut log = EnvelopeBuilder::new();
    let mut state = SessionState::default();
    let mut seqs = Vec::new();
    for f in frames {
        let env = log.envelope(Dir::Internal, f.clone());
        seqs.push(env.seq());
        state.apply(&env);
    }
    (state, seqs)
}

fn prompt() -> Value {
    json!({ "jsonrpc": "2.0", "id": "p1", "method": "session/prompt",
            "params": { "sessionId": "a", "prompt": [{ "type": "text", "text": "go" }] } })
}

#[test]
fn subagents_are_listed_in_the_order_they_started() {
    let (state, _) = fold(&[
        prompt(),
        frame(&started("a", "Explore", Some("one"))),
        frame(&started("b", "Plan", None)),
    ]);
    let ids: Vec<&str> = state.subagents().iter().map(Subagent::id).collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn a_repeated_block_keeps_its_wait_and_a_new_one_starts_another() {
    let block = frame(&blocked("a", "Explore", BlockKind::Permission, "Bash: ls"));
    let (state, seqs) = fold(&[
        prompt(),
        frame(&started("a", "Explore", None)),
        block.clone(),
        block,
    ]);
    // The second identical block didn't restart the wait the user saw at the first.
    assert!(!state.attention(seqs[2]));

    let (state, seqs) = fold(&[
        prompt(),
        frame(&started("a", "Explore", None)),
        frame(&blocked("a", "Explore", BlockKind::Permission, "Bash: ls")),
        frame(&blocked("a", "Explore", BlockKind::Question, "Which?")),
    ]);
    assert!(state.attention(seqs[2]));
}

#[test]
fn a_block_of_an_unknown_kind_still_wants_the_user() {
    let (state, _) = fold(&[
        prompt(),
        frame(&started("a", "Explore", None)),
        frame(&blocked(
            "a",
            "Explore",
            BlockKind::Other("telepathy".to_owned()),
            "?",
        )),
    ]);
    assert!(state.attention(Seq::ZERO));
}

#[test]
fn working_and_finished_for_an_unknown_subagent_change_nothing() {
    let (state, _) = fold(&[prompt(), frame(&working("x")), frame(&finished("y"))]);
    assert!(state.subagents().is_empty());
}

#[test]
fn a_start_again_keeps_what_the_first_one_knew() {
    let (state, _) = fold(&[
        prompt(),
        frame(&started("a", "Explore", Some("task"))),
        frame(&blocked("a", "Explore", BlockKind::Permission, "Bash: ls")),
        frame(&started("a", "Explore", None)),
    ]);
    let sub = &state.subagents()[0];
    assert_eq!(sub.task(), Some("task"));
    assert_eq!(
        sub.status(),
        &blocked_status(BlockKind::Permission, "Bash: ls")
    );
}

#[test]
fn an_exited_agent_has_no_subagents_and_takes_none() {
    let exited = json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/exited", "params": {} });
    let (state, _) = fold(&[
        prompt(),
        frame(&started("a", "Explore", None)),
        exited,
        frame(&started("b", "Explore", None)),
        frame(&blocked("c", "Explore", BlockKind::Permission, "Bash: ls")),
    ]);
    assert!(state.subagents().is_empty());
    assert!(!state.attention(Seq::ZERO));
}

#[test]
fn subagent_frames_round_trip_and_parse_typed() {
    let cases = [
        json!({ "id": "a", "agent_type": "Explore", "task": "t" }),
        json!({ "id": "a", "agent_type": "Explore" }),
    ];
    for params in cases {
        let raw =
            json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/started", "params": params });
        let f = Frame::from_value(raw.clone()).unwrap();
        assert!(
            matches!(&f, Frame::Shepherd(m) if matches!(m.body(), ShepherdFrame::SubagentStarted(_)))
        );
        assert_eq!(f.raw(), &raw);
    }
    let raw = json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/blocked",
                      "params": { "id": "a", "kind": "telepathy", "text": "?" } });
    let Frame::Shepherd(m) = Frame::from_value(raw).unwrap() else {
        panic!("not a shepherd frame")
    };
    let ShepherdFrame::SubagentBlocked(b) = m.body() else {
        panic!("not blocked")
    };
    assert_eq!(b.kind, BlockKind::Other("telepathy".to_owned()));
    assert_eq!(b.agent_type, None);
    assert_eq!(
        frame(&blocked("a", "E", BlockKind::Question, "q"))["params"],
        json!({ "id": "a", "kind": "question", "text": "q", "agent_type": "E" })
    );
    assert_eq!(
        frame(&finished("a")),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/finished", "params": { "id": "a" } })
    );
    assert_eq!(
        frame(&working("a")),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/subagent/working", "params": { "id": "a" } })
    );
}

// ---------- carried translators ----------

#[test]
fn a_translator_carried_from_a_build_without_subagents_still_restores() {
    let mut t = Translators::new();
    let session = SessionId::new("s_old");
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let payload = match agent {
            AgentKind::Codex => cx!("UserPromptSubmit"),
            _ => cc!("UserPromptSubmit"),
        };
        t.translate(&session, agent, &payload, || "seed".to_owned());
        let mut carried = serde_json::to_value(t.carry()).unwrap();
        let state = &mut carried["sessions"]["s_old"]["state"];
        let removed = state.as_object_mut().unwrap().remove("subagents");
        assert!(removed.is_some(), "{agent:?}: {state}");
        let (restored, dropped) = Translators::restore(serde_json::from_value(carried).unwrap());
        assert!(dropped.is_empty(), "{agent:?}");
        assert_eq!(restored.len(), 1);
    }
}
