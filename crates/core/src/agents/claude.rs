//! Claude Code hook payloads → ACP++ frames.
//!
//! Hook names and payload fields follow the Claude Code hooks reference (<https://code.claude.com/docs/en/hooks>,
//! read 2026-10-29); `crates/core/tests/fixtures/claude/README.md` says which fixture fields are documented.
//!
//! | hook | frames |
//! |---|---|
//! | `UserPromptSubmit` | `session/prompt` request (`to_agent`) + `user_message_chunk` |
//! | `PreToolUse` | `tool_call` `in_progress`, `toolCallId` = `tool_use_id`; after a continuation prompt if needed |
//! | `PostToolUse` | `tool_call_update` `completed` |
//! | `PostToolUseFailure`, `PermissionDenied` | `tool_call_update` `failed` |
//! | `PermissionRequest` | `session/request_permission` (`from_agent`); after a continuation prompt if needed |
//! | `Notification` `permission_prompt` | `session/request_permission`, unless one is already open |
//! | `Notification` `idle_prompt` | `_shepherd/agent/idle` |
//! | `Stop` | the open prompt's response with `end_turn` (`from_agent`), or `_shepherd/agent/idle` if none is open |
//! | `StopFailure` | `_claude-code/StopFailure` + `_shepherd/agent/idle` |
//! | `SessionStart` | `_shepherd/agent/started { agent_session_id }`, except after compaction (`source: compact`) |
//! | `SessionEnd` | `_shepherd/agent/exited`, except for `/clear` and `/resume`, which start a new session in the same process |
//! | `PreCompact` / `PostCompact` | `compaction_update` `in_progress` / `completed`, same `compactionId`; a new `PreCompact` first cancels one left open |
//! | `SubagentStart` | `_shepherd/subagent/started { id, agent_type, task? }`, `task` from the `Agent` tool call that spawned it; an internal agent (empty `agent_type`) is a vendor frame |
//! | `PostToolUse` of `Agent` naming an announced subagent with another task | also a corrective `_shepherd/subagent/started` |
//! | `SubagentStop` | `_shepherd/subagent/finished { id }`; an internal agent's is a vendor frame |
//! | a subagent's `PermissionRequest`, or `PreToolUse` of `AskUserQuestion` | also `_shepherd/subagent/blocked { id, kind, text }` (`permission`, or `question`) |
//! | a blocked subagent's next `PreToolUse`, `PostToolUse`, `PostToolUseFailure` or `PermissionDenied` | also `_shepherd/subagent/working { id }` |
//! | anything else | `_claude-code/<hook_name>` with the raw payload (`_claude-code/unknown` without a usable name) |
//!
//! Synthesized ACP frames carry `_meta.shepherd.synthesized`. A known hook missing a field its frames need becomes
//! its vendor frame instead.
//!
//! Continuations: when a main-agent `PreToolUse` or `PermissionRequest` (no `agent_id`) arrives with no prompt
//! open, a `Stop` hook kept the turn going after `Stop` fired. The translator opens an empty `session/prompt`
//! marked `_meta.shepherd.continuation: true`, which the next `Stop` answers.
//!
//! Subagents: hooks fired inside a subagent carry its `agent_id` and `agent_type`. `SubagentStart` has no task, so
//! it takes the task (`description`, else `prompt`) of the `Agent` tool call (`Task` in older Claude Code) that
//! spawned it, paired by the subagent id the call's `PostToolUse` names (`tool_response.agentId`, for `completed`
//! and `async_launched` alike), else by `subagent_type` (default `general-purpose`) when that is unambiguous; see
//! `agents::subagents`. Claude Code's own internal agents (prompt suggestions, `/btw`) fire `SubagentStop` with an
//! empty `agent_type`: they are not the session's subagents, so their hooks stay vendor frames.
//!
//! Interrupts: Claude Code fires no hook when the user presses Esc (`Stop` "does not run if the stoppage occurred
//! due to a user interrupt"). The turn ends at the next `UserPromptSubmit` (a new prompt replaces the old turn in
//! the fold) or at an `idle_prompt` notification. The reference documents `idle_prompt` as "about 60 seconds" after
//! Claude finishes responding with no typing since, but not whether an interrupted turn counts.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::acpp::acp::{
    CompactionStatus, CompactionUpdate, ContentBlock, ContentChunk, PromptRequest, PromptResponse,
    RequestPermissionRequest, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::acpp::{
    AgentExited, AgentIdle, AgentStarted, BlockKind, Dir, Frame, FrameDraft, RequestId,
    ShepherdFrame, Source, SourceKind,
};
use crate::ids::SessionId;

use super::subagents::{Subagents, ToolOutcome};

/// The agent name in `source.agent` and `_shepherd/agent/started`.
pub const AGENT: &str = "claude-code";

/// The hooks a Claude Code session is launched with: every hook the table above maps to more than a vendor frame.
pub const HOOK_EVENTS: [&str; 15] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "StopFailure",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
];

/// The tool that spawns a subagent: `Agent`, or `Task` in older Claude Code.
const SPAWN_TOOLS: [&str; 2] = ["Agent", "Task"];

/// The subagent type the `Agent` tool spawns when its input names none.
const DEFAULT_AGENT_TYPE: &str = "general-purpose";

/// The tool a subagent asks the user a question with.
const QUESTION_TOOL: &str = "AskUserQuestion";

/// What the translator remembers between hooks of one shepherd session. The host keeps one per session and passes
/// it to every [`translate`] call, so a prompt and its `Stop` pair up by id. It serializes, so the host can persist
/// it across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeTranslation {
    session: SessionId,
    /// Unique per translator lifetime within the session's log; prefixes every id this translator creates.
    id_seed: String,
    /// Claude Code's own session id, from the latest `SessionStart`.
    agent_session_id: Option<String>,
    /// Numbers the requests and compactions this translator creates.
    next_id: u64,
    /// The id of the open `session/prompt` request.
    prompt: Option<String>,
    /// A permission prompt was sent and no tool activity or turn end has followed.
    permission_open: bool,
    /// Tools that started (`PreToolUse`) and haven't finished in this turn, oldest first.
    tools: Vec<OpenTool>,
    /// The `compactionId` of a compaction that started and hasn't completed.
    compaction: Option<String>,
    #[serde(default)]
    subagents: Subagents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OpenTool {
    id: String,
    name: String,
    input: Option<Value>,
}

impl ClaudeTranslation {
    /// A fresh translator for shepherd session `session`.
    ///
    /// `id_seed` must be unique among the translators that ever write to this session's log, for example the
    /// session log's last seq or the newest envelope id when the translator is created. Request and compaction
    /// ids are `claude-<id_seed>-<kind>-<n>`, so a new translator for an existing session (after a host restart
    /// that didn't restore the old state) never reuses an id that is still in the log.
    pub fn new(session: SessionId, id_seed: impl Into<String>) -> Self {
        Self {
            session,
            id_seed: id_seed.into(),
            agent_session_id: None,
            next_id: 1,
            prompt: None,
            permission_open: false,
            tools: Vec::new(),
            compaction: None,
            subagents: Subagents::default(),
        }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    fn new_id(&mut self, kind: &str) -> String {
        let id = format!("claude-{}-{kind}-{}", self.id_seed, self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Forgets the turn in progress.
    fn end_turn(&mut self) {
        self.prompt = None;
        self.permission_open = false;
        self.tools.clear();
        self.subagents.end_turn();
    }

    fn draft(&self, dir: Dir, frame: Frame) -> FrameDraft {
        FrameDraft::new(
            frame,
            Source::new(SourceKind::Hook, Some(AGENT.to_owned())),
            dir,
            Some(self.session.clone()),
        )
    }
}

/// Translates one hook payload into the frames to append, updating `state`. Never panics: a payload it can't read
/// becomes a `_claude-code/*` frame carrying it.
pub fn translate(state: &mut ClaudeTranslation, payload: &Value) -> Vec<FrameDraft> {
    let Some(name) = hook_name(payload) else {
        return vendor(state, "unknown", payload);
    };
    let hook = Hook { payload };
    let frames = match name {
        "UserPromptSubmit" => user_prompt_submit(state, &hook),
        "PreToolUse" => pre_tool_use(state, &hook),
        "PostToolUse" => tool_done(
            state,
            &hook,
            ToolCallStatus::Completed,
            hook.field("tool_response"),
        ),
        "PostToolUseFailure" => {
            tool_done(state, &hook, ToolCallStatus::Failed, hook.field("error"))
        }
        "PermissionDenied" => tool_done(state, &hook, ToolCallStatus::Failed, hook.field("reason")),
        "PermissionRequest" => permission_request(state, &hook),
        "Notification" => notification(state, &hook),
        "Stop" => stop(state, &hook),
        "StopFailure" => stop_failure(state, &hook),
        "SessionStart" => session_start(state, &hook),
        "SessionEnd" => session_end(state, &hook),
        "PreCompact" => pre_compact(state, &hook),
        "PostCompact" => post_compact(state, &hook),
        "SubagentStart" => subagent_start(state, &hook),
        "SubagentStop" => subagent_stop(state, &hook),
        _ => None,
    };
    match frames {
        Some(frames) => frames
            .into_iter()
            .map(|(dir, frame)| state.draft(dir, frame))
            .collect(),
        None => vendor(state, name, payload),
    }
}

/// The frames for one hook, with their directions. `None` means "send the vendor frame instead".
type Frames = Option<Vec<(Dir, Frame)>>;

/// A hook payload, with field accessors.
struct Hook<'a> {
    payload: &'a Value,
}

impl<'a> Hook<'a> {
    fn field(&self, key: &str) -> Option<&'a Value> {
        self.payload.get(key)
    }

    fn str(&self, key: &str) -> Option<&'a str> {
        self.field(key).and_then(Value::as_str)
    }
}

/// `hook_event_name`, if it is a non-empty identifier that can go into a method name.
pub(crate) fn hook_name(payload: &Value) -> Option<&str> {
    payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// `_claude-code/<hook>` carrying the payload: as the params if it is an object, else under `payload`.
fn vendor(state: &ClaudeTranslation, hook: &str, payload: &Value) -> Vec<FrameDraft> {
    vec![state.draft(Dir::FromAgent, vendor_frame(hook, payload))]
}

fn vendor_frame(hook: &str, payload: &Value) -> Frame {
    let params = match payload {
        Value::Object(_) => payload.clone(),
        other => json!({ "payload": other }),
    };
    Frame::notification(&format!("_claude-code/{hook}"), params)
}

/// A synthesized `session/update`. `None` only if marking fails, which a `_meta`-less notification never does.
fn update(agent_session: &str, update: SessionUpdate) -> Option<Frame> {
    SessionNotification {
        session_id: agent_session.to_owned(),
        update,
        meta: None,
    }
    .to_frame()
    .into_synthesized()
    .ok()
}

fn agent_idle() -> Frame {
    ShepherdFrame::AgentIdle(AgentIdle::default()).to_frame()
}

/// The agent session id ACP frames carry: the payload's, else the last `SessionStart`'s.
fn agent_session(state: &ClaudeTranslation, hook: &Hook) -> String {
    hook.str("session_id")
        .or(state.agent_session_id.as_deref())
        .unwrap_or_default()
        .to_owned()
}

fn user_prompt_submit(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let agent_session = agent_session(state, hook);
    let text = hook.str("prompt");
    let mut out = vec![open_prompt(state, &agent_session, text, false)?];
    if let Some(text) = text {
        let chunk = SessionUpdate::UserMessageChunk(ContentChunk {
            content: ContentBlock::text(text),
            message_id: None,
            meta: None,
        });
        out.push((Dir::FromAgent, update(&agent_session, chunk)?));
    }
    Some(out)
}

/// Starts a turn with a new `session/prompt` request, which the next `Stop` answers. A `continuation` (work a
/// Stop hook kept going, not a user prompt) is marked `_meta.shepherd.continuation: true`.
fn open_prompt(
    state: &mut ClaudeTranslation,
    agent_session: &str,
    text: Option<&str>,
    continuation: bool,
) -> Option<(Dir, Frame)> {
    let id = state.new_id("prompt");
    let frame = PromptRequest {
        session_id: agent_session.to_owned(),
        prompt: text.map(ContentBlock::text).into_iter().collect(),
        meta: continuation.then(|| json!({ "shepherd": { "continuation": true } })),
    }
    .to_frame(RequestId::from(id.clone()))
    .into_synthesized()
    .ok()?;
    state.end_turn();
    state.prompt = Some(id);
    Some((Dir::ToAgent, frame))
}

/// The continuation prompt a main-agent tool hook needs when no prompt is open, else nothing.
///
/// A `Stop` hook (another one, or `/goal`) can block `Stop`, and Claude then keeps working after the `Stop` this
/// translator already answered, firing `Stop` again (`stop_hook_active: true`) when it is done. The fold ignores
/// tool activity while idle, so the continued work gets a prompt of its own: empty, marked
/// `_meta.shepherd.continuation`, answered by the next `Stop`. Subagent hooks (with `agent_id`) belong to the main
/// agent's turn and never open one.
fn continuation(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    if state.prompt.is_some() || hook.field("agent_id").is_some() {
        return Some(Vec::new());
    }
    let agent_session = agent_session(state, hook);
    Some(vec![open_prompt(state, &agent_session, None, true)?])
}

fn pre_tool_use(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let id = hook.str("tool_use_id")?;
    let mut out = continuation(state, hook)?;
    let name = hook.str("tool_name").unwrap_or("Tool");
    let input = hook.field("tool_input");
    let call = SessionUpdate::ToolCall(ToolCall {
        tool_call_id: id.to_owned(),
        title: tool_title(name, input),
        name: None,
        kind: tool_kind(name),
        status: ToolCallStatus::InProgress,
        raw_input: input.cloned(),
        meta: None,
    });
    let frame = update(&agent_session(state, hook), call)?;
    state.tools.retain(|t| t.id != id);
    state.tools.push(OpenTool {
        id: id.to_owned(),
        name: name.to_owned(),
        input: input.cloned(),
    });
    state.permission_open = false;
    out.push((Dir::FromAgent, frame));
    if SPAWN_TOOLS.contains(&name) {
        let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
        let agent_type = field("subagent_type").unwrap_or(DEFAULT_AGENT_TYPE);
        let task = field("description").or(field("prompt"));
        state.subagents.spawn(id, agent_type, task);
    }
    if let Some(agent) = hook.str("agent_id") {
        let frame = if name == QUESTION_TOOL {
            let text = question(input).unwrap_or_else(|| tool_title(name, input));
            let kind = BlockKind::Question;
            state
                .subagents
                .block(agent, hook.str("agent_type"), kind, text)
        } else {
            state.subagents.active(agent)
        };
        out.extend(frame.map(|f| (Dir::Internal, f)));
    }
    Some(out)
}

/// A tool call that finished with `status` (`completed` or `failed`).
fn tool_done(
    state: &mut ClaudeTranslation,
    hook: &Hook,
    status: ToolCallStatus,
    raw_output: Option<&Value>,
) -> Frames {
    let id = hook.str("tool_use_id")?;
    // A completed or background-launched `Agent` call names its subagent; an older `Task` result may not.
    let outcome = match status {
        ToolCallStatus::Completed => ToolOutcome::Completed {
            spawned: raw_output
                .and_then(|r| r.get("agentId"))
                .and_then(Value::as_str),
        },
        _ => ToolOutcome::Failed,
    };
    let patch = SessionUpdate::ToolCallUpdate(ToolCallUpdate {
        tool_call_id: id.to_owned(),
        title: None,
        kind: None,
        status: Some(status),
        raw_output: raw_output.cloned(),
        meta: None,
    });
    let frame = update(&agent_session(state, hook), patch)?;
    state.tools.retain(|t| t.id != id);
    state.permission_open = false;
    let mut out = vec![(Dir::FromAgent, frame)];
    let corrected = state.subagents.tool_done(id, outcome);
    out.extend(corrected.map(|f| (Dir::Internal, f)));
    if let Some(agent) = hook.str("agent_id") {
        out.extend(state.subagents.active(agent).map(|f| (Dir::Internal, f)));
    }
    Some(out)
}

/// `PermissionRequest` has no `tool_use_id`, so it names the latest open tool with the same name and input.
fn permission_request(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let name = hook.str("tool_name").unwrap_or("Tool");
    let input = hook.field("tool_input");
    let tool = state
        .tools
        .iter()
        .rev()
        .find(|t| t.name == name && t.input.as_ref() == input)
        .map(|t| t.id.clone());
    let mut out = continuation(state, hook)?;
    out.extend(permission(state, hook, tool, &tool_title(name, input))?);
    if let Some(agent) = hook.str("agent_id") {
        let (kind, text) = match question(input).filter(|_| name == QUESTION_TOOL) {
            Some(text) => (BlockKind::Question, text),
            None => (BlockKind::Permission, tool_title(name, input)),
        };
        let frame = state
            .subagents
            .block(agent, hook.str("agent_type"), kind, text);
        out.extend(frame.map(|f| (Dir::Internal, f)));
    }
    Some(out)
}

/// The first question of an `AskUserQuestion` input.
fn question(input: Option<&Value>) -> Option<String> {
    input?
        .get("questions")?
        .get(0)?
        .get("question")?
        .as_str()
        .map(str::to_owned)
}

/// A subagent started. An empty `agent_type` is one of Claude Code's internal agents, not the session's subagent.
fn subagent_start(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let id = hook.str("agent_id")?;
    let agent_type = hook.str("agent_type").filter(|t| !t.is_empty())?;
    Some(vec![(Dir::Internal, state.subagents.start(id, agent_type))])
}

fn subagent_stop(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let id = hook.str("agent_id")?;
    hook.str("agent_type").filter(|t| !t.is_empty())?;
    Some(vec![(Dir::Internal, state.subagents.finish(id))])
}

fn notification(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    match hook.str("notification_type")? {
        // Sent about six seconds after the dialog appears, usually after its `PermissionRequest`.
        "permission_prompt" if !state.permission_open => {
            let title = hook
                .str("message")
                .or(hook.str("title"))
                .unwrap_or("Permission needed")
                .to_owned();
            let tool = state.tools.last().map(|t| t.id.clone());
            permission(state, hook, tool, &title)
        }
        "idle_prompt" => {
            state.end_turn();
            Some(vec![(Dir::Internal, agent_idle())])
        }
        _ => None,
    }
}

/// A `session/request_permission` for `tool`, or for its own id when no open tool matches. The fold then clears
/// it on that tool's activity, or on any tool activity. The options stay empty: hooks don't say what the dialog
/// offers.
fn permission(
    state: &mut ClaudeTranslation,
    hook: &Hook,
    tool: Option<String>,
    title: &str,
) -> Frames {
    let id = state.new_id("permission");
    let tool_call_id = tool.unwrap_or_else(|| id.clone());
    let frame = RequestPermissionRequest {
        session_id: agent_session(state, hook),
        tool_call: ToolCallUpdate {
            tool_call_id,
            title: Some(title.to_owned()),
            kind: None,
            status: None,
            raw_output: None,
            meta: None,
        },
        options: json!([]),
        meta: None,
    }
    .to_frame(RequestId::from(id))
    .into_synthesized()
    .ok()?;
    state.permission_open = true;
    Some(vec![(Dir::FromAgent, frame)])
}

fn stop(state: &mut ClaudeTranslation, _hook: &Hook) -> Frames {
    let frame = match &state.prompt {
        Some(id) => {
            let end_turn = PromptResponse {
                stop_reason: StopReason::EndTurn,
                meta: None,
            };
            let frame = end_turn
                .to_frame(RequestId::from(id.as_str()))
                .into_synthesized()
                .ok()?;
            (Dir::FromAgent, frame)
        }
        None => (Dir::Internal, agent_idle()),
    };
    state.end_turn();
    Some(vec![frame])
}

/// An API error ended the turn. ACP would answer the prompt with a JSON-RPC error, but an error response has no
/// `result` to carry the synthesized mark, so the turn ends with idle and the payload is kept as a vendor frame.
fn stop_failure(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let out = vec![
        (Dir::FromAgent, vendor_frame("StopFailure", hook.payload)),
        (Dir::Internal, agent_idle()),
    ];
    state.end_turn();
    Some(out)
}

/// After compaction Claude Code fires `SessionStart` again in the same process, mid-turn; restarting the agent
/// there would drop the turn.
fn session_start(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    if hook.str("source") == Some("compact") {
        return None;
    }
    let agent_session_id = hook.str("session_id");
    let frame = ShepherdFrame::AgentStarted(AgentStarted {
        agent: AGENT.to_owned(),
        agent_session_id: agent_session_id.map(str::to_owned),
        pid: None,
        command: None,
        transcript_path: hook.str("transcript_path").map(str::to_owned),
    })
    .to_frame();
    state.end_turn();
    state.compaction = None;
    state.agent_session_id = agent_session_id.map(str::to_owned);
    Some(vec![(Dir::Internal, frame)])
}

/// `/clear` and `/resume` end one Claude Code session and start another in the same process, so they aren't
/// an exit. The `SessionStart` that follows restarts the agent.
fn session_end(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    if matches!(hook.str("reason"), Some("clear" | "resume")) {
        return None;
    }
    let frame = ShepherdFrame::AgentExited(AgentExited::default()).to_frame();
    state.end_turn();
    state.compaction = None;
    Some(vec![(Dir::Internal, frame)])
}

/// A hook can block `PreCompact`, and then no `PostCompact` follows. A compaction still open when the next one
/// starts was blocked, so it is cancelled first.
fn pre_compact(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let agent_session = agent_session(state, hook);
    let mut out = Vec::new();
    if let Some(old) = &state.compaction {
        let cancel = compaction(old, CompactionStatus::Cancelled, None);
        out.push((Dir::FromAgent, update(&agent_session, cancel)?));
    }
    let id = state.new_id("compaction");
    let start = compaction(&id, CompactionStatus::InProgress, None);
    out.push((Dir::FromAgent, update(&agent_session, start)?));
    state.compaction = Some(id);
    Some(out)
}

fn post_compact(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let id = state.compaction.as_deref()?;
    let done = compaction(id, CompactionStatus::Completed, hook.str("compact_summary"));
    let frame = update(&agent_session(state, hook), done)?;
    state.compaction = None;
    Some(vec![(Dir::FromAgent, frame)])
}

fn compaction(id: &str, status: CompactionStatus, summary: Option<&str>) -> SessionUpdate {
    SessionUpdate::CompactionUpdate(CompactionUpdate {
        compaction_id: id.to_owned(),
        status,
        summary: summary.map(|s| json!([ContentBlock::text(s)])),
        error: None,
    })
}

/// `<tool>: <the first line of its main argument>`, or just the tool name.
fn tool_title(name: &str, input: Option<&Value>) -> String {
    const MAIN_ARGS: &[&str] = &[
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "query",
        "description",
    ];
    let arg = input.and_then(|input| {
        MAIN_ARGS
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .and_then(|s| s.lines().next())
            .filter(|s| !s.is_empty())
    });
    match arg {
        Some(arg) => format!("{name}: {arg}"),
        None => name.to_owned(),
    }
}

/// The ACP tool kind for a Claude Code tool.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" | "PowerShell" => ToolKind::Execute,
        "Read" => ToolKind::Read,
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => ToolKind::Edit,
        "Glob" | "Grep" => ToolKind::Search,
        "WebFetch" | "WebSearch" => ToolKind::Fetch,
        _ => ToolKind::default(),
    }
}
