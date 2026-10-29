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
//! | anything else | `_claude-code/<hook_name>` with the raw payload (`_claude-code/unknown` without a usable name) |
//!
//! Synthesized ACP frames carry `_meta.shepherd.synthesized`. A known hook missing a field its frames need becomes
//! its vendor frame instead.
//!
//! Continuations: when a main-agent `PreToolUse` or `PermissionRequest` (no `agent_id`) arrives with no prompt
//! open, a `Stop` hook kept the turn going after `Stop` fired. The translator opens an empty `session/prompt`
//! marked `_meta.shepherd.continuation: true`, which the next `Stop` answers.
//!
//! Interrupts: Claude Code fires no hook when the user presses Esc (`Stop` "does not run if the stoppage occurred
//! due to a user interrupt"). The turn ends at the next `UserPromptSubmit` (a new prompt replaces the old turn in
//! the fold) or at an `idle_prompt` notification. The reference documents `idle_prompt` as "about 60 seconds" after
//! Claude finishes responding with no typing since, but not whether an interrupted turn counts.

mod frames;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acpp::{Dir, Frame, FrameDraft, RequestId, Source, SourceKind};
use crate::ids::SessionId;

/// The agent name in `source.agent` and `_shepherd/agent/started`.
pub const AGENT: &str = "claude-code";

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
        "PostToolUse" => tool_done(state, &hook, "completed", hook.field("tool_response")),
        "PostToolUseFailure" => tool_done(state, &hook, "failed", hook.field("error")),
        "PermissionDenied" => tool_done(state, &hook, "failed", hook.field("reason")),
        "PermissionRequest" => permission_request(state, &hook),
        "Notification" => notification(state, &hook),
        "Stop" => stop(state, &hook),
        "StopFailure" => stop_failure(state, &hook),
        "SessionStart" => session_start(state, &hook),
        "SessionEnd" => session_end(state, &hook),
        "PreCompact" => pre_compact(state, &hook),
        "PostCompact" => post_compact(state, &hook),
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
fn hook_name(payload: &Value) -> Option<&str> {
    payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn vendor(state: &ClaudeTranslation, hook: &str, payload: &Value) -> Vec<FrameDraft> {
    frames::vendor(hook, payload)
        .map(|f| state.draft(Dir::FromAgent, f))
        .into_iter()
        .collect()
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
        out.push((
            Dir::FromAgent,
            frames::user_message_chunk(&agent_session, text)?,
        ));
    }
    Some(out)
}

/// Starts a turn with a new `session/prompt` request, which the next `Stop` answers.
fn open_prompt(
    state: &mut ClaudeTranslation,
    agent_session: &str,
    text: Option<&str>,
    continuation: bool,
) -> Option<(Dir, Frame)> {
    let id = state.new_id("prompt");
    let frame = frames::prompt(
        &RequestId::from(id.clone()),
        agent_session,
        text,
        continuation,
    )?;
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
    let frame = frames::tool_call(
        &agent_session(state, hook),
        id,
        &tool_title(name, input),
        tool_kind(name),
        input,
    )?;
    state.tools.retain(|t| t.id != id);
    state.tools.push(OpenTool {
        id: id.to_owned(),
        name: name.to_owned(),
        input: input.cloned(),
    });
    state.permission_open = false;
    out.push((Dir::FromAgent, frame));
    Some(out)
}

fn tool_done(
    state: &mut ClaudeTranslation,
    hook: &Hook,
    status: &str,
    raw_output: Option<&Value>,
) -> Frames {
    let id = hook.str("tool_use_id")?;
    let frame = frames::tool_call_done(&agent_session(state, hook), id, status, raw_output)?;
    state.tools.retain(|t| t.id != id);
    state.permission_open = false;
    Some(vec![(Dir::FromAgent, frame)])
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
    Some(out)
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
            Some(vec![(Dir::Internal, frames::agent_idle()?)])
        }
        _ => None,
    }
}

/// A `session/request_permission` for `tool`, or for its own id when no open tool matches. The fold then clears
/// it on that tool's activity, or on any tool activity.
fn permission(
    state: &mut ClaudeTranslation,
    hook: &Hook,
    tool: Option<String>,
    title: &str,
) -> Frames {
    let id = state.new_id("permission");
    let tool_call_id = tool.unwrap_or_else(|| id.clone());
    let frame = frames::request_permission(
        &RequestId::from(id),
        &agent_session(state, hook),
        &tool_call_id,
        title,
    )?;
    state.permission_open = true;
    Some(vec![(Dir::FromAgent, frame)])
}

fn stop(state: &mut ClaudeTranslation, _hook: &Hook) -> Frames {
    let frame = match &state.prompt {
        Some(id) => (
            Dir::FromAgent,
            frames::prompt_end_turn(&RequestId::from(id.as_str()))?,
        ),
        None => (Dir::Internal, frames::agent_idle()?),
    };
    state.end_turn();
    Some(vec![frame])
}

/// An API error ended the turn. ACP would answer the prompt with a JSON-RPC error, but an error response has no
/// `result` to carry the synthesized mark, so the turn ends with idle and the payload is kept as a vendor frame.
fn stop_failure(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let out = vec![
        (Dir::FromAgent, frames::vendor("StopFailure", hook.payload)?),
        (Dir::Internal, frames::agent_idle()?),
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
    let frame = frames::agent_started(AGENT, agent_session_id)?;
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
    let frame = frames::agent_exited()?;
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
        out.push((
            Dir::FromAgent,
            frames::compaction(&agent_session, old, "cancelled", None)?,
        ));
    }
    let id = state.new_id("compaction");
    out.push((
        Dir::FromAgent,
        frames::compaction(&agent_session, &id, "in_progress", None)?,
    ));
    state.compaction = Some(id);
    Some(out)
}

fn post_compact(state: &mut ClaudeTranslation, hook: &Hook) -> Frames {
    let id = state.compaction.as_deref()?;
    let frame = frames::compaction(
        &agent_session(state, hook),
        id,
        "completed",
        hook.str("compact_summary"),
    )?;
    state.compaction = None;
    Some(vec![(Dir::FromAgent, frame)])
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
fn tool_kind(name: &str) -> &'static str {
    match name {
        "Bash" | "PowerShell" => "execute",
        "Read" => "read",
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "edit",
        "Glob" | "Grep" => "search",
        "WebFetch" | "WebSearch" => "fetch",
        _ => "other",
    }
}
