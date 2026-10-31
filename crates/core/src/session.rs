//! The per-session fold: agent activity, turns, and attention, derived from a session's ACP++ log.
//!
//! `session_state = session_log.fold(SessionState::default(), apply)`. Frames the fold doesn't read change nothing.

use crate::acpp::acp::{
    AcpFrame, ContentBlock, PromptRequest, RequestPermissionRequest, SessionUpdate, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::acpp::{
    BlockKind, Correlated, Dir, Envelope, Frame, PendingRequests, RequestId, Response, Seq,
    ShepherdFrame, SubagentBlocked, SubagentStarted,
};

/// What the agent in a session is doing, as far as its log tells.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    /// The session exists but the agent hasn't started a turn or gone idle yet. The state before any frame.
    Starting,
    /// A turn is running. `tool` is the most recently started tool call that is still in flight.
    Working { tool: Option<ToolSummary> },
    /// The agent asked to use a tool and is waiting for the user's answer. `title` comes from the request, else
    /// from the in-flight tool it names, else is its `toolCallId`.
    ///
    /// The user answers in the agent's own TUI, so the answer is usually inferred. The request's own response,
    /// a prompt response, `_shepherd/agent/idle`, or a new prompt always clear it. Otherwise it waits on the
    /// request's `toolCallId`: if that was an in-flight tool when the request arrived, only a `tool_call` or
    /// `tool_call_update` for that id clears it; if it wasn't (typical for hook-synthesized requests), any
    /// `tool_call` or `tool_call_update` does.
    NeedsPermission { title: String },
    /// The agent asked the user a question. No frame in shepherd's ACP subset produces this yet.
    NeedsInput,
    /// The agent is at its prompt with nothing to do.
    Idle,
    /// The agent process ended. Only `_shepherd/agent/started` leaves this state.
    Exited { code: Option<i32> },
    /// The log doesn't say. No frame produces this yet.
    Unknown,
}

/// A tool call an agent is running.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSummary {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
}

impl ToolSummary {
    fn from_call(call: &ToolCall) -> Self {
        Self {
            id: call.tool_call_id.clone(),
            title: call.title.clone(),
            kind: call.kind.clone(),
        }
    }
}

/// A subagent of the session's agent, from the `_shepherd/subagent/*` frames. Its text is the agent's, raw: display
/// it only through `display_safe`.
#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    id: String,
    agent_type: String,
    task: Option<String>,
    status: SubagentStatus,
    /// The seq of the block behind [`SubagentStatus::Blocked`].
    blocked_since: Option<Seq>,
}

impl Subagent {
    /// The agent's own id for it.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent_type(&self) -> &str {
        &self.agent_type
    }

    /// What it was asked to do, if the hooks said.
    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    pub fn status(&self) -> &SubagentStatus {
        &self.status
    }

    fn waits_since(&self) -> Option<Seq> {
        match self.status {
            SubagentStatus::Blocked { .. } => self.blocked_since,
            _ => None,
        }
    }
}

/// Where a subagent is.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStatus {
    Working,
    /// Waiting on the user, who answers in the parent session's TUI.
    Blocked {
        kind: BlockKind,
        text: String,
    },
    /// Done; listed until the parent's turn ends.
    Finished,
}

/// A request still waiting for its response: its direction and id, to forget it in `pending` once its outcome
/// is inferred.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    dir: Dir,
    id: RequestId,
}

/// The permission request behind [`Activity::NeedsPermission`].
#[derive(Debug, Clone, PartialEq)]
struct Permission {
    request: Request,
    /// The in-flight tool it names, if it named one when it arrived.
    waits_on: Option<String>,
}

/// One session's state, folded from its log.
///
/// Only a prompt request starts a turn: `tool_call` and `tool_call_update` frames that arrive while the session is
/// `Starting` or `Idle` are ignored, because a `Working` with no idle ever coming is worse than a missed one.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    activity: Activity,
    /// Tool calls in flight in this turn, oldest first.
    tools: Vec<ToolSummary>,
    permission: Option<Permission>,
    prompt: Option<Request>,
    /// The seq at which the current wait on the user ([`Activity::Idle`] or [`Activity::NeedsPermission`]) began.
    waiting_since: Option<Seq>,
    /// Holds at most the current prompt and permission requests.
    pending: PendingRequests,
    turns: u32,
    last_prompt: Option<String>,
    /// The agent's subagents since its turn began, in the order they started.
    subagents: Vec<Subagent>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            activity: Activity::Starting,
            tools: Vec::new(),
            permission: None,
            prompt: None,
            waiting_since: None,
            pending: PendingRequests::default(),
            turns: 0,
            last_prompt: None,
            subagents: Vec::new(),
        }
    }
}

impl SessionState {
    /// Folds one envelope from this session's log.
    pub fn apply(&mut self, env: &Envelope) {
        let frame = env.frame();
        match frame {
            Frame::Shepherd(m) => self.apply_shepherd(env.seq(), m.body()),
            _ if matches!(self.activity, Activity::Exited { .. }) => {}
            Frame::Acp(m) => {
                let request = m.id().map(|id| Request {
                    dir: env.dir().clone(),
                    id: id.clone(),
                });
                match (m.body(), request) {
                    (AcpFrame::Prompt(p), Some(request)) => {
                        self.prompt(p, request, frame.is_continuation());
                        self.pending.record(env.dir(), frame);
                    }
                    (AcpFrame::RequestPermission(r), Some(request)) => {
                        self.request_permission(env.seq(), r, request);
                        self.pending.record(env.dir(), frame);
                    }
                    (AcpFrame::Update(n), _) => match &n.update {
                        SessionUpdate::ToolCall(call) => self.tool_call(call),
                        SessionUpdate::ToolCallUpdate(update) => self.tool_update(update),
                        _ => {}
                    },
                    _ => {}
                }
            }
            Frame::Response(r) => self.response(env, r),
            Frame::Unknown(_) => {}
        }
    }

    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Tool calls in flight in the current turn, oldest first.
    pub fn tools(&self) -> &[ToolSummary] {
        &self.tools
    }

    /// Whether the session wants the user: it, or one of its subagents, started waiting on them (blocked, or idle)
    /// after `last_seen`, the last seq the user has seen. A wait the user has already seen doesn't want them again.
    /// A blocked subagent wants the user whatever the agent itself is doing.
    pub fn attention(&self, last_seen: Seq) -> bool {
        let own = match self.activity {
            Activity::NeedsPermission { .. } | Activity::NeedsInput | Activity::Idle => {
                self.waiting_since.is_some_and(|seq| seq > last_seen)
            }
            _ => false,
        };
        own || self
            .subagents
            .iter()
            .any(|s| s.waits_since().is_some_and(|seq| seq > last_seen))
    }

    /// The agent's subagents in its current turn (and any that report in after it), in the order they started.
    /// Finished ones stay until the turn ends; the turn ending drops them all, including any that never reported
    /// finishing.
    pub fn subagents(&self) -> &[Subagent] {
        &self.subagents
    }

    /// Whether a subagent is waiting on the user.
    pub fn subagent_blocked(&self) -> bool {
        self.subagents
            .iter()
            .any(|s| matches!(s.status, SubagentStatus::Blocked { .. }))
    }

    /// How many prompts the user has sent. Continuation prompts (`_meta.shepherd.continuation: true`, e.g. a turn
    /// a Claude Code Stop hook kept going) open a turn for activity but aren't counted.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// The text of the latest user prompt that had any, with its text blocks joined by newlines. Continuation
    /// prompts never change it.
    pub fn last_prompt(&self) -> Option<&str> {
        self.last_prompt.as_deref()
    }

    fn apply_shepherd(&mut self, seq: Seq, frame: &ShepherdFrame) {
        match frame {
            ShepherdFrame::AgentStarted(_) => {
                // A new agent process: nothing in flight from the old one still stands.
                *self = Self {
                    turns: self.turns,
                    last_prompt: self.last_prompt.take(),
                    ..Self::default()
                };
            }
            ShepherdFrame::AgentIdle(_) if !matches!(self.activity, Activity::Exited { .. }) => {
                self.go_idle(seq)
            }
            ShepherdFrame::AgentExited(e) => {
                *self = Self {
                    activity: Activity::Exited { code: e.code },
                    turns: self.turns,
                    last_prompt: self.last_prompt.take(),
                    ..Self::default()
                };
            }
            _ if matches!(self.activity, Activity::Exited { .. }) => {}
            ShepherdFrame::SubagentStarted(s) => self.subagent_started(s),
            ShepherdFrame::SubagentBlocked(b) => self.subagent_blocked_at(seq, b),
            ShepherdFrame::SubagentWorking(w) => {
                if let Some(sub) = self.subagent_mut(&w.id)
                    && matches!(sub.status, SubagentStatus::Blocked { .. })
                {
                    sub.status = SubagentStatus::Working;
                }
            }
            ShepherdFrame::SubagentFinished(f) => {
                if let Some(sub) = self.subagent_mut(&f.id) {
                    sub.status = SubagentStatus::Finished;
                }
            }
            _ => {}
        }
    }

    fn subagent_mut(&mut self, id: &str) -> Option<&mut Subagent> {
        self.subagents.iter_mut().find(|s| s.id == id)
    }

    /// A start for a subagent already listed corrects it: its type and task are replaced (a start without a task
    /// retracts a guessed one), and where it is stays.
    fn subagent_started(&mut self, started: &SubagentStarted) {
        if let Some(sub) = self.subagent_mut(&started.id) {
            sub.agent_type = started.agent_type.clone();
            sub.task = started.task.clone();
            return;
        }
        self.subagents.push(Subagent {
            id: started.id.clone(),
            agent_type: started.agent_type.clone(),
            task: started.task.clone(),
            status: SubagentStatus::Working,
            blocked_since: None,
        });
    }

    /// A block lists a subagent the session doesn't have. The same block again keeps the wait it began.
    fn subagent_blocked_at(&mut self, seq: Seq, block: &SubagentBlocked) {
        let status = SubagentStatus::Blocked {
            kind: block.kind.clone(),
            text: block.text.clone(),
        };
        if let Some(sub) = self.subagent_mut(&block.id) {
            if sub.status != status {
                sub.status = status;
                sub.blocked_since = Some(seq);
            }
            return;
        }
        self.subagents.push(Subagent {
            id: block.id.clone(),
            agent_type: block.agent_type.clone().unwrap_or_default(),
            task: None,
            status,
            blocked_since: Some(seq),
        });
    }

    /// Opens a turn. A `continuation` isn't a user prompt, so it leaves `turns` and `last_prompt` alone.
    fn prompt(&mut self, prompt: &PromptRequest, request: Request, continuation: bool) {
        if !continuation {
            self.user_prompt(prompt);
        }
        self.end_turn();
        self.prompt = Some(request);
        self.work();
    }

    fn user_prompt(&mut self, prompt: &PromptRequest) {
        self.turns = self.turns.saturating_add(1);
        let text: Vec<&str> = prompt
            .prompt
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                ContentBlock::Other(_) => None,
            })
            .collect();
        if !text.is_empty() {
            self.last_prompt = Some(text.join("\n"));
        }
    }

    fn tool_call(&mut self, call: &ToolCall) {
        if !self.in_turn() {
            return;
        }
        self.tools.retain(|t| t.id != call.tool_call_id);
        if !is_done(&call.status) {
            self.tools.push(ToolSummary::from_call(call));
        }
        self.tool_activity(&call.tool_call_id);
    }

    fn tool_update(&mut self, update: &ToolCallUpdate) {
        if !self.in_turn() {
            return;
        }
        if update.status.as_ref().is_some_and(is_done) {
            self.tools.retain(|t| t.id != update.tool_call_id);
        } else if let Some(tool) = self.tools.iter_mut().find(|t| t.id == update.tool_call_id) {
            if let Some(title) = &update.title {
                tool.title = title.clone();
            }
            if let Some(kind) = &update.kind {
                tool.kind = kind.clone();
            }
        }
        self.tool_activity(&update.tool_call_id);
    }

    /// After a frame about tool `id`: clears a permission waiting on it (or on no known tool), then shows the
    /// current tool unless a permission is still pending.
    fn tool_activity(&mut self, id: &str) {
        if let Some(p) = &self.permission
            && p.waits_on.as_deref().is_none_or(|w| w == id)
        {
            self.forget_permission();
        }
        if self.permission.is_none() {
            self.work();
        }
    }

    fn request_permission(&mut self, seq: Seq, request: &RequestPermissionRequest, req: Request) {
        let call = &request.tool_call;
        let known = self.tools.iter().find(|t| t.id == call.tool_call_id);
        let title = call
            .title
            .clone()
            .or_else(|| known.map(|t| t.title.clone()))
            .unwrap_or_else(|| call.tool_call_id.clone());
        let waits_on = known.map(|t| t.id.clone());
        self.forget_permission();
        self.permission = Some(Permission {
            request: req,
            waits_on,
        });
        self.activity = Activity::NeedsPermission { title };
        self.waiting_since = Some(seq);
    }

    fn response(&mut self, env: &Envelope, response: &Response) {
        match self.pending.correlate(env.dir(), response) {
            Correlated::Prompt(_) => {
                self.prompt = None;
                self.go_idle(env.seq());
            }
            Correlated::Other { .. } => {
                // Only the current permission is pending, so this answers it.
                if self.permission.take().is_some() {
                    self.work();
                }
            }
            Correlated::Unmatched => {}
        }
    }

    fn in_turn(&self) -> bool {
        matches!(
            self.activity,
            Activity::Working { .. } | Activity::NeedsPermission { .. }
        )
    }

    /// Working on the latest tool in flight, if any.
    fn work(&mut self) {
        self.activity = Activity::Working {
            tool: self.tools.last().cloned(),
        };
        self.waiting_since = None;
    }

    /// Drops the in-flight state of the current turn, forgetting requests whose responses no longer matter.
    fn end_turn(&mut self) {
        self.tools.clear();
        self.subagents.clear();
        self.forget_permission();
        if let Some(r) = self.prompt.take() {
            self.pending.forget(&r.dir, &r.id);
        }
    }

    /// Clears the current permission, whose outcome was inferred rather than answered.
    fn forget_permission(&mut self) {
        if let Some(p) = self.permission.take() {
            self.pending.forget(&p.request.dir, &p.request.id);
        }
    }

    /// Idle from `seq`, unless already idle: a second idle signal for the same rest doesn't reset attention.
    fn go_idle(&mut self, seq: Seq) {
        self.end_turn();
        if self.activity != Activity::Idle {
            self.activity = Activity::Idle;
            self.waiting_since = Some(seq);
        }
    }
}

fn is_done(status: &ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}
