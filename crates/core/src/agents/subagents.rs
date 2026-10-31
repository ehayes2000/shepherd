//! What a translator remembers about the agent's subagents between hooks, and the `_shepherd/subagent/*` frames it
//! writes. Shared by [`super::claude`] and [`super::codex`], whose hooks report subagents the same way:
//!
//! - The spawning tool call's `PreToolUse` (Claude Code's `Agent` tool, Codex's `spawn_agent`) comes first. It names
//!   the subagent's type and task, but not the id the subagent will get.
//! - `SubagentStart { agent_id, agent_type }` has no task. Its order against the spawning call's `PostToolUse` varies:
//!   Codex returns from `spawn_agent` before the child's `SubagentStart` runs, a Claude Code foreground subagent's
//!   `PostToolUse` comes when it is done, and a background one's when it is launched.
//! - The spawning call's `PostToolUse` names the subagent's id (Claude Code: `tool_response.agentId`; Codex: the
//!   `agent_id` of `SpawnAgentResult`). That pairs the call's task with the subagent exactly: kept for a
//!   `SubagentStart` still to come, or sent as a corrective `subagent/started` for one already announced (the fold
//!   takes the task of a repeated start).
//! - Before that, a `SubagentStart` guesses its task only when exactly one open spawning call has its type and no
//!   guess yet; otherwise it starts without one.
//! - Hooks fired inside a subagent carry its `agent_id` and `agent_type`; its permission requests and questions block
//!   it, and its next tool activity unblocks it.
//! - `SubagentStop { agent_id }` finishes it.

use serde::{Deserialize, Serialize};

use crate::acpp::{
    BlockKind, Frame, ShepherdFrame, SubagentBlocked, SubagentFinished, SubagentStarted,
    SubagentWorking,
};

/// A translator's view of the agent's subagents. It serializes with the translator; a translator carried from a
/// build without it starts with none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Subagents {
    /// Spawning tool calls this turn whose `PostToolUse` hasn't named their subagent yet, oldest first.
    spawns: Vec<Spawn>,
    /// Subagents a spawning call named before their `SubagentStart`, with the call's task.
    #[serde(default)]
    named: Vec<Named>,
    /// Subagents started this turn, with the task they were announced with.
    #[serde(default)]
    started: Vec<Named>,
    /// Subagents last reported blocked, with what on.
    blocked: Vec<Blocked>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Spawn {
    tool_use_id: String,
    agent_type: String,
    task: Option<String>,
    /// The subagent a `SubagentStart` guessed was this call's.
    #[serde(default)]
    guessed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Named {
    id: String,
    agent_type: String,
    task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Blocked {
    id: String,
    kind: BlockKind,
    text: String,
}

impl Subagents {
    /// A spawning tool call `tool_use_id` started, for a subagent of `agent_type` asked to do `task`.
    pub(crate) fn spawn(&mut self, tool_use_id: &str, agent_type: &str, task: Option<&str>) {
        self.spawns.retain(|s| s.tool_use_id != tool_use_id);
        self.spawns.push(Spawn {
            tool_use_id: tool_use_id.to_owned(),
            agent_type: agent_type.to_owned(),
            task: task.map(str::to_owned),
            guessed: None,
        });
    }

    /// Tool call `tool_use_id` finished, naming `agent` as the subagent it spawned if it is a spawning call that did.
    /// A corrective started frame when that subagent was already announced with another task.
    pub(crate) fn tool_done(&mut self, tool_use_id: &str, agent: Option<&str>) -> Option<Frame> {
        let i = self
            .spawns
            .iter()
            .position(|s| s.tool_use_id == tool_use_id)?;
        let spawn = self.spawns.remove(i);
        let agent = agent?;
        if let Some(started) = self.started.iter_mut().find(|s| s.id == agent) {
            if spawn.task.is_none() || started.task == spawn.task {
                return None;
            }
            started.task = spawn.task;
            return Some(started_frame(started));
        }
        self.named.retain(|n| n.id != agent);
        self.named.push(Named {
            id: agent.to_owned(),
            agent_type: spawn.agent_type,
            task: spawn.task,
        });
        None
    }

    /// `SubagentStart`: the started frame, with the task of the spawning call that named it, or of the one open
    /// spawning call of its type.
    pub(crate) fn start(&mut self, id: &str, agent_type: &str) -> Frame {
        let task = match self.named.iter().position(|n| n.id == id) {
            Some(i) => self.named.remove(i).task,
            None => self.guess(id, agent_type),
        };
        self.blocked.retain(|b| b.id != id);
        self.started.retain(|s| s.id != id);
        let started = Named {
            id: id.to_owned(),
            agent_type: agent_type.to_owned(),
            task,
        };
        let frame = started_frame(&started);
        self.started.push(started);
        frame
    }

    /// The task of the only open, unguessed spawning call of `agent_type`, which is then guessed for `id`.
    fn guess(&mut self, id: &str, agent_type: &str) -> Option<String> {
        let mut open = self
            .spawns
            .iter_mut()
            .filter(|s| s.agent_type == agent_type && s.guessed.is_none());
        let only = open.next()?;
        if open.next().is_some() {
            return None;
        }
        only.guessed = Some(id.to_owned());
        only.task.clone()
    }

    /// Subagent `id` is waiting on the user. `None` when it already was, on the same thing.
    pub(crate) fn block(
        &mut self,
        id: &str,
        agent_type: Option<&str>,
        kind: BlockKind,
        text: String,
    ) -> Option<Frame> {
        let block = Blocked {
            id: id.to_owned(),
            kind,
            text,
        };
        if self.blocked.contains(&block) {
            return None;
        }
        self.blocked.retain(|b| b.id != id);
        let frame = ShepherdFrame::SubagentBlocked(SubagentBlocked {
            id: id.to_owned(),
            kind: block.kind.clone(),
            text: block.text.clone(),
            agent_type: agent_type.map(str::to_owned),
        })
        .to_frame();
        self.blocked.push(block);
        Some(frame)
    }

    /// Subagent `id` showed tool activity: the working frame if it was blocked, else nothing.
    pub(crate) fn active(&mut self, id: &str) -> Option<Frame> {
        let was_blocked = self.blocked.iter().any(|b| b.id == id);
        self.blocked.retain(|b| b.id != id);
        was_blocked.then(|| {
            ShepherdFrame::SubagentWorking(SubagentWorking { id: id.to_owned() }).to_frame()
        })
    }

    /// `SubagentStop`: the finished frame.
    pub(crate) fn finish(&mut self, id: &str) -> Frame {
        self.blocked.retain(|b| b.id != id);
        ShepherdFrame::SubagentFinished(SubagentFinished { id: id.to_owned() }).to_frame()
    }

    /// The parent's turn ended: the session fold drops its subagents, so this forgets them too.
    pub(crate) fn end_turn(&mut self) {
        self.spawns.clear();
        self.named.clear();
        self.started.clear();
        self.blocked.clear();
    }
}

fn started_frame(started: &Named) -> Frame {
    ShepherdFrame::SubagentStarted(SubagentStarted {
        id: started.id.clone(),
        agent_type: started.agent_type.clone(),
        task: started.task.clone(),
    })
    .to_frame()
}
