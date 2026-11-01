//! The per-session fold: agent activity, turns, and attention, derived from a session's ACP++ log.
//!
//! `session_state = session_log.fold(SessionState::default(), apply)`. Frames the fold doesn't read change nothing.

use crate::acpp::acp::{
    AcpFrame, ContentBlock, PromptRequest, RequestPermissionRequest, SessionUpdate, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::acpp::{
    AgentAsked, BlockKind, Correlated, Dir, Envelope, Frame, PendingRequests, PromptBy, RequestId,
    Response, Seq, SessionAnswered, ShepherdFrame, SourceKind, SubagentBlocked, SubagentStarted,
    Timestamp,
};

/// What the agent in a session is doing, as far as its log tells.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The agent asked the user a question (`_shepherd/agent/asked` of a `question` with no permission request
    /// open). It waits on the tool that asks it, and clears like a permission.
    NeedsInput,
    /// The agent is at its prompt with nothing to do.
    Idle,
    /// The agent process ended. Only `_shepherd/agent/started` leaves this state.
    Exited { code: Option<i32> },
    /// The log doesn't say. No frame produces this yet.
    Unknown,
}

/// A tool call an agent is running.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    dir: Dir,
    id: RequestId,
}

/// What the agent waits on the user for: the permission request behind [`Activity::NeedsPermission`], or the
/// question behind [`Activity::NeedsInput`]. Whatever clears one clears the other.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Permission {
    /// The `session/request_permission` it came with; none for a question asked without one.
    request: Option<Request>,
    /// The in-flight tool it names, if it named one when it arrived.
    waits_on: Option<String>,
    /// What the agent asked (`_shepherd/agent/asked`), with that entry's seq; none before the asked frame arrives.
    asked: Option<(Seq, AgentAsked)>,
    /// The user typed into the session since it was asked: they are answering it in the pane, so shepherd won't.
    in_pane: bool,
}

/// The last prompt the host typed into a session's agent (`_shepherd/session/prompted`), and who asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    pub text: String,
    pub by: PromptBy,
    /// Its entry's seq and `ts`: what came after it is the agent's answer.
    pub seq: Seq,
    pub at: Timestamp,
}

/// One session's state, folded from its log.
///
/// Only a prompt request starts a turn: `tool_call` and `tool_call_update` frames that arrive while the session is
/// `Starting` or `Idle` are ignored, because a `Working` with no idle ever coming is worse than a missed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    activity: Activity,
    /// Tool calls in flight in this turn, oldest first.
    tools: Vec<ToolSummary>,
    permission: Option<Permission>,
    /// Dialogs opened before the current wait that no activity has shown closed yet (the agents queue dialogs, so one
    /// may still be up in front): each the tool it waits on, whose activity shows it closed; or none for one on no
    /// known tool, which nothing but the turn ending shows closed.
    covered: Vec<Option<String>>,
    prompt: Option<Request>,
    /// The seq at which the current wait on the user ([`Activity::Idle`] or [`Activity::NeedsPermission`]) began.
    waiting_since: Option<Seq>,
    /// Holds at most the current prompt and permission requests.
    pending: PendingRequests,
    turns: u32,
    last_prompt: Option<String>,
    /// The agent's subagents since its turn began, in the order they started.
    subagents: Vec<Subagent>,
    /// The agent reported its own start (an `agent/started` from its hooks, not the host's on resume), so a fresh
    /// agent that hasn't had a turn yet is at its prompt.
    reported: bool,
    told: Option<Told>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            activity: Activity::Starting,
            tools: Vec::new(),
            permission: None,
            covered: Vec::new(),
            prompt: None,
            waiting_since: None,
            pending: PendingRequests::default(),
            turns: 0,
            last_prompt: None,
            subagents: Vec::new(),
            reported: false,
            told: None,
        }
    }
}

impl SessionState {
    /// Folds one envelope from this session's log.
    pub fn apply(&mut self, env: &Envelope) {
        let frame = env.frame();
        match frame {
            Frame::Shepherd(m) => self.apply_shepherd(env, m.body()),
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

    /// Whether the agent is at its prompt, ready to be typed into: idle, or started (as its hooks reported) and not
    /// yet in a turn, and no subagent of its is waiting on the user (whose dialog would take the keys).
    pub fn at_prompt(&self) -> bool {
        let ready = match self.activity {
            Activity::Idle => true,
            Activity::Starting => self.reported,
            _ => false,
        };
        ready && !self.subagent_blocked()
    }

    /// Whether the agent waits on the user: it asked permission or a question, or a subagent of its did.
    pub fn wants_user(&self) -> bool {
        matches!(
            self.activity,
            Activity::NeedsPermission { .. } | Activity::NeedsInput
        ) || self.subagent_blocked()
    }

    /// What the agent asked the user, if it waits on them and said: the permission's title, or the question, else the
    /// first blocked subagent's text. Raw: display it only through `display_safe`.
    pub fn asked(&self) -> Option<&str> {
        let question = self
            .permission
            .as_ref()
            .and_then(|p| p.asked.as_ref())
            .map(|(_, q)| q);
        match (&self.activity, question) {
            (Activity::NeedsPermission { title }, _) => Some(title),
            (Activity::NeedsInput, Some(question)) => Some(&question.text),
            _ => self.subagents.iter().find_map(|s| match &s.status {
                SubagentStatus::Blocked { text, .. } => Some(text.as_str()),
                _ => None,
            }),
        }
    }

    /// The question the agent is blocked on that shepherd may answer (`_shepherd/agent/asked`), if any. None while the
    /// user is answering it in the pane, and while another dialog the agent opened before it may still be up in front
    /// of it (the agents queue them): until the agent's activity shows each closed, keys could land in that one.
    pub fn question(&self) -> Option<&AgentAsked> {
        self.pending_question().map(|(_, q)| q)
    }

    /// The seq of the `_shepherd/agent/asked` entry behind [`SessionState::question`]: it names that question, so an
    /// answer meant for it can't land on a later one.
    pub fn question_seq(&self) -> Option<Seq> {
        self.pending_question().map(|(seq, _)| seq)
    }

    fn pending_question(&self) -> Option<(Seq, &AgentAsked)> {
        let wait = self.permission.as_ref()?;
        if wait.in_pane || !self.covered.is_empty() {
            return None;
        }
        wait.asked.as_ref().map(|(seq, q)| (*seq, q))
    }

    /// The last prompt the host typed into the agent, if any.
    pub fn told(&self) -> Option<&Told> {
        self.told.as_ref()
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

    fn apply_shepherd(&mut self, env: &Envelope, frame: &ShepherdFrame) {
        let seq = env.seq();
        match frame {
            ShepherdFrame::AgentStarted(_) => {
                // A new agent process: nothing in flight from the old one still stands. The host's own (a resume)
                // comes before the agent runs; the agent's hooks say when it has started.
                *self = Self {
                    turns: self.turns,
                    last_prompt: self.last_prompt.take(),
                    told: self.told.take(),
                    reported: env.source().kind != SourceKind::Shepherd,
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
                    told: self.told.take(),
                    ..Self::default()
                };
            }
            ShepherdFrame::SessionPrompted(prompted) => {
                self.told = Some(Told {
                    text: prompted.text.clone(),
                    by: prompted.by.clone(),
                    seq,
                    at: env.ts().clone(),
                });
                // The host typed it in: the turn has started, though the agent's own prompt signal comes later.
                if !matches!(self.activity, Activity::Exited { .. }) {
                    self.end_turn();
                    self.work();
                }
            }
            _ if matches!(self.activity, Activity::Exited { .. }) => {}
            ShepherdFrame::AgentAsked(asked) => self.agent_asked(seq, asked),
            ShepherdFrame::SessionAnswered(answered) => self.answered(answered),
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

    /// What the agent asks: it describes the permission it is waiting on, if that has no description yet; a question
    /// with nothing waiting waits on the user itself, on the latest tool in flight (the tool that asks it). Another
    /// question while one is up is queued behind it ([`SessionState::cover`]). Outside a turn there is nothing to ask
    /// about.
    fn agent_asked(&mut self, seq: Seq, asked: &AgentAsked) {
        if let Some(wait) = &self.permission {
            match &wait.asked {
                Some((_, same)) if same == asked => {}
                Some(_) => {
                    let waits_on = self.tools.last().map(|t| t.id.clone());
                    let in_pane = self.cover();
                    self.permission = Some(Permission {
                        request: None,
                        waits_on,
                        asked: Some((seq, asked.clone())),
                        in_pane,
                    });
                }
                None => {
                    if let Some(wait) = &mut self.permission {
                        wait.asked = Some((seq, asked.clone()));
                    }
                }
            }
            return;
        }
        if asked.kind == BlockKind::Permission || !self.in_turn() {
            return;
        }
        self.permission = Some(Permission {
            request: None,
            waits_on: self.tools.last().map(|t| t.id.clone()),
            asked: Some((seq, asked.clone())),
            in_pane: false,
        });
        self.activity = Activity::NeedsInput;
        self.waiting_since = Some(seq);
    }

    /// Before another dialog opens over the open wait: the agents queue dialogs and keep showing the first, so the
    /// open one is remembered in `covered` until its tool's activity shows it closed (or the turn ends), and nothing is
    /// answerable meanwhile. Forgets the open wait; returns whether the user had typed into it, which carries over.
    fn cover(&mut self) -> bool {
        let Some(open) = &self.permission else {
            return false;
        };
        let in_pane = open.in_pane;
        let waits_on = open.waits_on.clone();
        self.still_covers(waits_on);
        self.forget_permission();
        in_pane
    }

    /// Remembers a dialog that may still be up in front, once: one per tool, and one for every dialog on no known tool.
    fn still_covers(&mut self, waits_on: Option<String>) {
        if !self.covered.contains(&waits_on) {
            self.covered.push(waits_on);
        }
    }

    /// An answer shepherd typed ends the wait, and a subagent that asked works on. Without a choice the user is
    /// answering in the pane: the agent still waits, but the question is no longer shepherd's to answer.
    fn answered(&mut self, answered: &SessionAnswered) {
        let Some(wait) = &mut self.permission else {
            return;
        };
        if answered.choice.is_none() {
            wait.in_pane = true;
            return;
        }
        let subagent = wait.asked.as_ref().and_then(|(_, a)| a.subagent.clone());
        self.forget_permission();
        if let Some(sub) = subagent.and_then(|id| self.subagent_mut(&id))
            && matches!(sub.status, SubagentStatus::Blocked { .. })
        {
            sub.status = SubagentStatus::Working;
        }
        self.work();
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
        // A covered dialog for a known tool is shown closed by that tool's activity; one for no known tool never is.
        self.covered
            .retain(|w| w.as_deref().is_none_or(|w| w != id));
        if let Some(p) = &self.permission
            && p.waits_on.as_deref().is_none_or(|w| w == id)
        {
            // A wait on no known tool clears on any activity, which doesn't prove its dialog closed: it stays
            // covered (uncertain) until the turn ends, so no later dialog is answered into it.
            if p.waits_on.is_none() {
                self.still_covers(None);
            }
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
        // The same tool's request (e.g. after its question's `PreToolUse`) is the dialog already up; any other is
        // queued behind it.
        let same = self
            .permission
            .as_ref()
            .is_some_and(|p| p.waits_on.is_some() && p.waits_on == waits_on);
        let in_pane = match same {
            true => self.permission.as_ref().is_some_and(|p| p.in_pane),
            false => self.cover(),
        };
        self.forget_permission();
        self.permission = Some(Permission {
            request: Some(req),
            waits_on,
            asked: None,
            in_pane,
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
            Activity::Working { .. } | Activity::NeedsPermission { .. } | Activity::NeedsInput
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
        self.covered.clear();
        self.forget_permission();
        if let Some(r) = self.prompt.take() {
            self.pending.forget(&r.dir, &r.id);
        }
    }

    /// Clears the current permission, whose outcome was inferred rather than answered.
    fn forget_permission(&mut self) {
        if let Some(r) = self.permission.take().and_then(|p| p.request) {
            self.pending.forget(&r.dir, &r.id);
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
