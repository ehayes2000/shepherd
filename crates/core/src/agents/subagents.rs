//! What a translator remembers about the agent's subagents between hooks, and the `_shepherd/subagent/*` frames it
//! writes. Shared by [`super::claude`] and [`super::codex`], whose hooks report subagents the same way:
//!
//! - The spawning tool call's `PreToolUse` (Claude Code's `Agent` tool, Codex's `spawn_agent`) comes first. It names
//!   the subagent's type and task, but not the id the subagent will get.
//! - `SubagentStart { agent_id, agent_type }` follows while that call runs. It has no task, so it takes the task of
//!   the oldest spawning call this turn with the same type that hasn't started a subagent or finished.
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
    /// Spawning tool calls this turn that haven't started a subagent yet, oldest first.
    spawns: Vec<Spawn>,
    /// Subagents last reported blocked, with what on.
    blocked: Vec<Blocked>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Spawn {
    tool_use_id: String,
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
        });
    }

    /// Tool call `tool_use_id` finished: a spawning call that never started a subagent won't now.
    pub(crate) fn tool_done(&mut self, tool_use_id: &str) {
        self.spawns.retain(|s| s.tool_use_id != tool_use_id);
    }

    /// `SubagentStart`: the started frame, with the task of the oldest open spawn of the same type.
    pub(crate) fn start(&mut self, id: &str, agent_type: &str) -> Frame {
        let task = self
            .spawns
            .iter()
            .position(|s| s.agent_type == agent_type)
            .and_then(|i| self.spawns.remove(i).task);
        self.blocked.retain(|b| b.id != id);
        ShepherdFrame::SubagentStarted(SubagentStarted {
            id: id.to_owned(),
            agent_type: agent_type.to_owned(),
            task,
        })
        .to_frame()
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
        self.blocked.clear();
    }
}
