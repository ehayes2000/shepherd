//! The per-session fold: agent activity, turns, and attention, derived from a session's ACP++ log.
//!
//! `session_state = session_log.fold(SessionState::default(), apply)`. Frames the fold doesn't read change nothing.

use crate::acpp::acp::{
    AcpFrame, ContentBlock, PromptRequest, RequestPermissionRequest, SessionUpdate, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
use crate::acpp::{Correlated, Envelope, Frame, PendingRequests, Response, Seq, ShepherdFrame};

/// What the agent in a session is doing, as far as its log tells.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    /// The session exists but the agent hasn't started a turn or gone idle yet. The state before any frame.
    Starting,
    /// A turn is running, optionally inside a tool call.
    Working { tool: Option<ToolSummary> },
    /// The agent asked to use a tool and is waiting for the user's answer.
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

/// The tool call an agent is running.
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

/// One session's state, folded from its log.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    activity: Activity,
    /// The tool call in flight, kept while [`Activity::NeedsPermission`] hides it.
    tool: Option<ToolSummary>,
    /// The seq at which the current [`Activity::Idle`] was reached.
    idle_since: Option<Seq>,
    pending: PendingRequests,
    turns: u32,
    last_prompt: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            activity: Activity::Starting,
            tool: None,
            idle_since: None,
            pending: PendingRequests::default(),
            turns: 0,
            last_prompt: None,
        }
    }
}

impl SessionState {
    /// Folds one envelope from this session's log.
    pub fn apply(&mut self, env: &Envelope) {
        let frame = env.frame();
        // Only the requests whose responses the fold reads, so other requests leave the state untouched.
        if let Frame::Acp(m) = frame
            && matches!(
                m.body(),
                AcpFrame::Prompt(_) | AcpFrame::RequestPermission(_)
            )
        {
            self.pending.record(env.dir(), frame);
        }
        match frame {
            Frame::Shepherd(m) => self.apply_shepherd(env.seq(), m.body()),
            _ if matches!(self.activity, Activity::Exited { .. }) => {}
            Frame::Acp(m) => match m.body() {
                AcpFrame::Prompt(p) => self.prompt(p),
                AcpFrame::Update(n) => match &n.update {
                    SessionUpdate::ToolCall(call) => self.tool_call(call),
                    SessionUpdate::ToolCallUpdate(update) => self.tool_update(update),
                    _ => {}
                },
                AcpFrame::RequestPermission(r) => self.request_permission(r),
                AcpFrame::Cancel(_) => {}
            },
            Frame::Response(r) => self.response(env, r),
            Frame::Unknown(_) => {}
        }
    }

    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Whether the session wants the user: it is blocked on them, or went idle after `last_seen`, the last seq
    /// the user has seen.
    pub fn attention(&self, last_seen: Seq) -> bool {
        match self.activity {
            Activity::NeedsPermission { .. } | Activity::NeedsInput => true,
            Activity::Idle => self.idle_since.is_some_and(|seq| seq > last_seen),
            _ => false,
        }
    }

    /// How many prompts the user has sent.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// The text of the latest prompt that had any, with its text blocks joined by newlines.
    pub fn last_prompt(&self) -> Option<&str> {
        self.last_prompt.as_deref()
    }

    fn apply_shepherd(&mut self, seq: Seq, frame: &ShepherdFrame) {
        match frame {
            ShepherdFrame::AgentStarted(_) => {
                // A new agent process: nothing in flight from the old one still stands.
                self.activity = Activity::Starting;
                self.tool = None;
                self.idle_since = None;
                self.pending = PendingRequests::default();
            }
            ShepherdFrame::AgentIdle(_) if !matches!(self.activity, Activity::Exited { .. }) => {
                self.go_idle(seq)
            }
            ShepherdFrame::AgentExited(e) => {
                self.activity = Activity::Exited { code: e.code };
                self.tool = None;
                self.idle_since = None;
            }
            _ => {}
        }
    }

    fn prompt(&mut self, prompt: &PromptRequest) {
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
        self.tool = None;
        self.work();
    }

    fn tool_call(&mut self, call: &ToolCall) {
        self.tool = (!is_done(&call.status)).then(|| ToolSummary::from_call(call));
        self.work();
    }

    fn tool_update(&mut self, update: &ToolCallUpdate) {
        if !matches!(
            self.activity,
            Activity::Working { .. } | Activity::NeedsPermission { .. }
        ) {
            return;
        }
        if let Some(tool) = self.tool.as_mut().filter(|t| t.id == update.tool_call_id) {
            if update.status.as_ref().is_some_and(is_done) {
                self.tool = None;
            } else {
                if let Some(title) = &update.title {
                    tool.title = title.clone();
                }
                if let Some(kind) = &update.kind {
                    tool.kind = kind.clone();
                }
            }
        }
        self.work();
    }

    fn request_permission(&mut self, request: &RequestPermissionRequest) {
        let call = &request.tool_call;
        let known = self.tool.as_ref().filter(|t| t.id == call.tool_call_id);
        let title = call
            .title
            .clone()
            .or_else(|| known.map(|t| t.title.clone()))
            .unwrap_or_else(|| call.tool_call_id.clone());
        if known.is_none() {
            self.tool = Some(ToolSummary {
                id: call.tool_call_id.clone(),
                title: title.clone(),
                kind: call.kind.clone().unwrap_or_default(),
            });
        }
        self.activity = Activity::NeedsPermission { title };
        self.idle_since = None;
    }

    fn response(&mut self, env: &Envelope, response: &Response) {
        match self.pending.correlate(env.dir(), response) {
            Correlated::Prompt(_) => self.go_idle(env.seq()),
            Correlated::Other { method } if method == "session/request_permission" => {
                if matches!(self.activity, Activity::NeedsPermission { .. }) {
                    self.work();
                }
            }
            Correlated::Other { .. } | Correlated::Unmatched => {}
        }
    }

    /// Working on the current tool, if any.
    fn work(&mut self) {
        self.activity = Activity::Working {
            tool: self.tool.clone(),
        };
        self.idle_since = None;
    }

    /// Idle from `seq`, unless already idle: a second idle signal for the same rest doesn't reset attention.
    fn go_idle(&mut self, seq: Seq) {
        self.tool = None;
        if self.activity != Activity::Idle {
            self.activity = Activity::Idle;
            self.idle_since = Some(seq);
        }
    }
}

fn is_done(status: &ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}
