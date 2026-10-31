//! Codex hook payloads → ACP++ frames, and the trust state Codex needs to run shepherd's hooks.
//!
//! Hook names and payload fields follow the Codex hooks reference (<https://developers.openai.com/codex/hooks>, now
//! <https://learn.chatgpt.com/docs/hooks>, read 2026-10-30) and the input structs in `codex-rs/hooks/src/schema.rs`
//! (openai/codex rust-v0.166.0); `crates/core/tests/fixtures/codex/README.md` says which fixture fields are
//! documented.
//!
//! | hook | frames |
//! |---|---|
//! | `UserPromptSubmit` | `session/prompt` request (`to_agent`) + `user_message_chunk` |
//! | `PreToolUse` | `tool_call` `in_progress`, `toolCallId` = `tool_use_id`; after a continuation prompt if needed |
//! | `PostToolUse` | `tool_call_update` `completed` |
//! | `PermissionRequest` | `session/request_permission` (`from_agent`); after a continuation prompt if needed |
//! | `Stop` | the open prompt's response with `end_turn`, or `_shepherd/agent/idle` if none is open |
//! | `Interrupt` | the open prompt's response with `cancelled`, or `_shepherd/agent/idle` if none is open |
//! | `SessionStart` | `_shepherd/agent/started { agent_session_id }`, except after compaction (`source: compact`) |
//! | `SessionEnd` | `_shepherd/agent/exited` |
//! | `PreCompact` / `PostCompact` | `compaction_update` `in_progress` / `completed`, same `compactionId` |
//! | anything else (`SubagentStart`, `SubagentStop`, ...) | `_codex/<hook_name>` with the raw payload (`_codex/unknown` without a usable name) |
//!
//! Synthesized ACP frames carry `_meta.shepherd.synthesized`. A known hook missing a field its frames need becomes
//! its vendor frame instead. Continuations work as for Claude Code (see [`super::claude`]): a Codex `Stop` hook can
//! block `Stop` too, and main-thread tool activity with no prompt open gets an empty continuation prompt.
//!
//! Codex has no `Notification` hook, so there is no idle notification and no permission-dialog notification; and no
//! `PostToolUseFailure`, so a failed tool still completes through `PostToolUse` with its output.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::acpp::acp::{
    CompactionStatus, CompactionUpdate, ContentBlock, ContentChunk, PromptRequest, PromptResponse,
    RequestPermissionRequest, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::acpp::{
    AgentExited, AgentIdle, AgentStarted, Dir, Frame, FrameDraft, RequestId, ShepherdFrame, Source,
    SourceKind,
};
use crate::ids::SessionId;

/// The agent name in `source.agent` and `_shepherd/agent/started`.
pub const AGENT: &str = "codex";

/// The hooks a Codex session is launched with: every hook the table above maps to more than a vendor frame.
pub const HOOK_EVENTS: [&str; 10] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "Interrupt",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
];

/// The `timeout` (seconds) of each shepherd hook. Explicit, because Codex hashes the normalized timeout; within the
/// 1..=3 s Codex allows for `SessionEnd` and `Interrupt`, and well above `shepherd hook`'s own one-second wait.
pub const HOOK_TIMEOUT_SECS: u64 = 3;

// ---------- hook trust ----------
//
// Codex (rust-v0.166.0) runs a non-managed hook only when `hooks.state."<key>".trusted_hash` equals the hook's
// current hash, and it reads that state from the session-flags layer (`-c`) as well as the user's config
// (`codex-rs/hooks/src/config_rules.rs`, `hook_states_from_stack`). Shepherd passes its hooks and their trust state
// together with `-c`, so only its own hooks are trusted and `~/.codex` is never written. If Codex changes the key or
// hash format, the hooks are skipped (untrusted) and Codex asks the user to review them in `/hooks`; nothing else
// runs that wouldn't have run anyway. Decision 0016.

/// The hook-state key Codex gives the one handler shepherd registers for `event` through `-c`: `hook_key` in
/// `codex-rs/hooks/src/lib.rs`, with the session-flags layer's key source from `config_toml_source_path` in
/// `codex-rs/hooks/src/engine/discovery.rs`. `None` for an event shepherd doesn't hook.
pub fn hook_trust_key(event: &str) -> Option<String> {
    Some(format!(
        "/<session-flags>/config.toml:{}:0:0",
        event_key_label(event)?
    ))
}

/// The hash Codex trusts shepherd's handler for `event` by: `hook_hash` in `codex-rs/hooks/src/engine/discovery.rs`
/// over the normalized handler, then `version_for_toml` in `codex-rs/config/src/fingerprint.rs` (sha256 of the
/// key-sorted compact JSON of the TOML value). `None` for an event shepherd doesn't hook.
pub fn hook_trust_hash(event: &str, command: &str) -> Option<String> {
    let identity = HookIdentity {
        event_name: event_key_label(event)?,
        hooks: vec![CommandHook::new(command)],
    };
    // Codex goes through a TOML value first; so does this, so integers and absent fields come out the same.
    let toml = toml::Value::try_from(&identity).ok()?;
    let mut json = serde_json::to_value(toml).ok()?;
    json.sort_all_objects();
    let bytes = serde_json::to_vec(&json).ok()?;
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("sha256:{hex}"))
}

/// The `hooks` table to pass with `-c hooks=<value>`: one matcher-less group per [`HOOK_EVENTS`] entry running
/// `command`, plus `state` trusting exactly those handlers.
/// `None` only if the config can't be built, which every event in [`HOOK_EVENTS`] having a label and a plain
/// command config always serializing rule out.
pub fn hooks_config(command: &str) -> Option<toml::Table> {
    let group = MatcherGroup {
        hooks: vec![CommandHook::new(command)],
    };
    let mut hooks = toml::Table::new();
    let mut state = toml::Table::new();
    for event in HOOK_EVENTS {
        let key = hook_trust_key(event)?;
        let hash = hook_trust_hash(event, command)?;
        hooks.insert(event.to_owned(), toml::Value::try_from([&group]).ok()?);
        let mut trusted = toml::Table::new();
        trusted.insert("trusted_hash".to_owned(), toml::Value::String(hash));
        state.insert(key, toml::Value::Table(trusted));
    }
    hooks.insert("state".to_owned(), toml::Value::Table(state));
    Some(hooks)
}

/// `hook_event_key_label` in `codex-rs/hooks/src/lib.rs`, for the events shepherd hooks.
fn event_key_label(event: &str) -> Option<&'static str> {
    Some(match event {
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "Stop" => "stop",
        "Interrupt" => "interrupt",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionEnd" => "session_end",
        _ => return None,
    })
}

/// `NormalizedHookIdentity` in `codex-rs/hooks/src/engine/discovery.rs`. No `matcher`: none of these events gets
/// one from shepherd.
#[derive(Serialize)]
struct HookIdentity {
    event_name: &'static str,
    hooks: Vec<CommandHook>,
}

/// A `MatcherGroup` without a matcher, so it fires for every tool and source.
#[derive(Serialize)]
struct MatcherGroup {
    hooks: Vec<CommandHook>,
}

/// A command handler as Codex normalizes it (`HookHandlerConfig::Command`): synchronous, so hooks reach the log in
/// order, with an explicit timeout and no Windows command or status message.
#[derive(Serialize)]
struct CommandHook {
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
    timeout: u64,
    #[serde(rename = "async")]
    run_async: bool,
}

impl CommandHook {
    fn new(command: &str) -> Self {
        Self {
            kind: "command",
            command: command.to_owned(),
            timeout: HOOK_TIMEOUT_SECS,
            run_async: false,
        }
    }
}

// ---------- translation ----------

/// What the translator remembers between hooks of one shepherd session, like
/// [`ClaudeTranslation`](super::claude::ClaudeTranslation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexTranslation {
    session: SessionId,
    /// Unique per translator lifetime within the session's log; prefixes every id this translator creates.
    id_seed: String,
    /// Codex's own session id, from the latest `SessionStart`.
    agent_session_id: Option<String>,
    next_id: u64,
    /// The id of the open `session/prompt` request.
    prompt: Option<String>,
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

impl CodexTranslation {
    /// A fresh translator for shepherd session `session`. `id_seed` must be unique among the translators that ever
    /// write to this session's log (see [`ClaudeTranslation::new`](super::claude::ClaudeTranslation::new)).
    pub fn new(session: SessionId, id_seed: impl Into<String>) -> Self {
        Self {
            session,
            id_seed: id_seed.into(),
            agent_session_id: None,
            next_id: 1,
            prompt: None,
            tools: Vec::new(),
            compaction: None,
        }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    fn new_id(&mut self, kind: &str) -> String {
        let id = format!("codex-{}-{kind}-{}", self.id_seed, self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn end_turn(&mut self) {
        self.prompt = None;
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
/// becomes a `_codex/*` frame carrying it.
pub fn translate(state: &mut CodexTranslation, payload: &Value) -> Vec<FrameDraft> {
    let Some(name) = hook_name(payload) else {
        return vec![state.draft(Dir::FromAgent, vendor_frame("unknown", payload))];
    };
    let hook = Hook { payload };
    let frames = match name {
        "UserPromptSubmit" => user_prompt_submit(state, &hook),
        "PreToolUse" => pre_tool_use(state, &hook),
        "PostToolUse" => post_tool_use(state, &hook),
        "PermissionRequest" => permission_request(state, &hook),
        "Stop" => turn_over(state, StopReason::EndTurn),
        "Interrupt" => turn_over(state, StopReason::Cancelled),
        "SessionStart" => session_start(state, &hook),
        "SessionEnd" => session_end(state),
        "PreCompact" => pre_compact(state, &hook),
        "PostCompact" => post_compact(state, &hook),
        _ => None,
    };
    match frames {
        Some(frames) => frames
            .into_iter()
            .map(|(dir, frame)| state.draft(dir, frame))
            .collect(),
        None => vec![state.draft(Dir::FromAgent, vendor_frame(name, payload))],
    }
}

/// The frames for one hook, with their directions. `None` means "send the vendor frame instead".
type Frames = Option<Vec<(Dir, Frame)>>;

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

/// `_codex/<hook>` carrying the payload: as the params if it is an object, else under `payload`.
fn vendor_frame(hook: &str, payload: &Value) -> Frame {
    let params = match payload {
        Value::Object(_) => payload.clone(),
        other => json!({ "payload": other }),
    };
    Frame::notification(&format!("_codex/{hook}"), params)
}

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

/// The agent session id ACP frames carry: the payload's, else the last `SessionStart`'s.
fn agent_session(state: &CodexTranslation, hook: &Hook) -> String {
    hook.str("session_id")
        .or(state.agent_session_id.as_deref())
        .unwrap_or_default()
        .to_owned()
}

fn user_prompt_submit(state: &mut CodexTranslation, hook: &Hook) -> Frames {
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

fn open_prompt(
    state: &mut CodexTranslation,
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

/// The continuation prompt a main-thread tool hook needs when no prompt is open, else nothing. Subagent hooks
/// (with `agent_id`) belong to the main thread's turn and never open one.
fn continuation(state: &mut CodexTranslation, hook: &Hook) -> Frames {
    if state.prompt.is_some() || hook.field("agent_id").is_some() {
        return Some(Vec::new());
    }
    let agent_session = agent_session(state, hook);
    Some(vec![open_prompt(state, &agent_session, None, true)?])
}

fn pre_tool_use(state: &mut CodexTranslation, hook: &Hook) -> Frames {
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
    out.push((Dir::FromAgent, frame));
    Some(out)
}

fn post_tool_use(state: &mut CodexTranslation, hook: &Hook) -> Frames {
    let id = hook.str("tool_use_id")?;
    let patch = SessionUpdate::ToolCallUpdate(ToolCallUpdate {
        tool_call_id: id.to_owned(),
        title: None,
        kind: None,
        status: Some(ToolCallStatus::Completed),
        raw_output: hook.field("tool_response").cloned(),
        meta: None,
    });
    let frame = update(&agent_session(state, hook), patch)?;
    state.tools.retain(|t| t.id != id);
    Some(vec![(Dir::FromAgent, frame)])
}

/// `PermissionRequest` has no `tool_use_id`, so it names the latest open tool with the same name and input. Codex
/// adds `tool_input.description` to the permission request, so that field is left out of the comparison.
fn permission_request(state: &mut CodexTranslation, hook: &Hook) -> Frames {
    let name = hook.str("tool_name").unwrap_or("Tool");
    let input = hook.field("tool_input");
    let tool = state
        .tools
        .iter()
        .rev()
        .find(|t| t.name == name && same_input(t.input.as_ref(), input))
        .map(|t| t.id.clone());
    let mut out = continuation(state, hook)?;
    let id = state.new_id("permission");
    let frame = RequestPermissionRequest {
        session_id: agent_session(state, hook),
        tool_call: ToolCallUpdate {
            tool_call_id: tool.unwrap_or_else(|| id.clone()),
            title: Some(tool_title(name, input)),
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
    out.push((Dir::FromAgent, frame));
    Some(out)
}

fn same_input(a: Option<&Value>, b: Option<&Value>) -> bool {
    fn without_description(v: Option<&Value>) -> Option<Value> {
        match v {
            Some(Value::Object(o)) => {
                let o: Map<String, Value> = o
                    .iter()
                    .filter(|(k, _)| *k != "description")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(Value::Object(o))
            }
            other => other.cloned(),
        }
    }
    without_description(a) == without_description(b)
}

/// `Stop` or `Interrupt`: the open prompt's response with `reason`, or idle when no prompt is open.
fn turn_over(state: &mut CodexTranslation, reason: StopReason) -> Frames {
    let frame = match &state.prompt {
        Some(id) => {
            let response = PromptResponse {
                stop_reason: reason,
                meta: None,
            };
            let frame = response
                .to_frame(RequestId::from(id.as_str()))
                .into_synthesized()
                .ok()?;
            (Dir::FromAgent, frame)
        }
        None => (
            Dir::Internal,
            ShepherdFrame::AgentIdle(AgentIdle::default()).to_frame(),
        ),
    };
    state.end_turn();
    Some(vec![frame])
}

/// After compaction Codex fires `SessionStart` again (`source: compact`) mid-turn; restarting the agent there would
/// drop the turn.
fn session_start(state: &mut CodexTranslation, hook: &Hook) -> Frames {
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

fn session_end(state: &mut CodexTranslation) -> Frames {
    let frame = ShepherdFrame::AgentExited(AgentExited::default()).to_frame();
    state.end_turn();
    state.compaction = None;
    Some(vec![(Dir::Internal, frame)])
}

/// A compaction still open when the next one starts never completed, so it is cancelled first.
fn pre_compact(state: &mut CodexTranslation, hook: &Hook) -> Frames {
    let agent_session = agent_session(state, hook);
    let mut out = Vec::new();
    if let Some(old) = &state.compaction {
        let cancel = compaction(old, CompactionStatus::Cancelled);
        out.push((Dir::FromAgent, update(&agent_session, cancel)?));
    }
    let id = state.new_id("compaction");
    let start = compaction(&id, CompactionStatus::InProgress);
    out.push((Dir::FromAgent, update(&agent_session, start)?));
    state.compaction = Some(id);
    Some(out)
}

/// Codex's `PostCompact` carries no summary.
fn post_compact(state: &mut CodexTranslation, hook: &Hook) -> Frames {
    let id = state.compaction.as_deref()?;
    let frame = update(
        &agent_session(state, hook),
        compaction(id, CompactionStatus::Completed),
    )?;
    state.compaction = None;
    Some(vec![(Dir::FromAgent, frame)])
}

fn compaction(id: &str, status: CompactionStatus) -> SessionUpdate {
    SessionUpdate::CompactionUpdate(CompactionUpdate {
        compaction_id: id.to_owned(),
        status,
        summary: None,
        error: None,
    })
}

/// `Bash: <first line of the command>`, `apply_patch: <first file the patch touches>`, or just the tool name.
fn tool_title(name: &str, input: Option<&Value>) -> String {
    let command = input.and_then(|i| i.get("command")).and_then(Value::as_str);
    let arg = match (name, command) {
        ("apply_patch", Some(patch)) => patch.lines().find_map(|line| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        }),
        (_, Some(command)) => command.lines().next(),
        (_, None) => None,
    };
    match arg.map(str::trim).filter(|s| !s.is_empty()) {
        Some(arg) => format!("{name}: {arg}"),
        None => name.to_owned(),
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" => ToolKind::Execute,
        "apply_patch" => ToolKind::Edit,
        _ => ToolKind::default(),
    }
}
