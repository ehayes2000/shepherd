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
//!   `SubagentStart` still to come, in this turn or a later one, or sent as a corrective `subagent/started` for one
//!   already announced this turn (the fold replaces a subagent's task with a repeated start's).
//! - A `SubagentStart` for a subagent already started this turn keeps the task it has. One whose id no remembered
//!   call named guesses its task only when exactly one call of its type is a candidate and that candidate is an open
//!   call. Candidates are the open calls not guessed yet, and the completed calls no start has claimed (their result
//!   named no id, or one no start has used yet, e.g. in another format). Else it starts without a task. A failed call
//!   stops being a candidate at once; a completed one stays until a start claims its id, or it is forgotten (its
//!   subagent's `SubagentStop`, the [`MAX_NAMED`] bound, or the turn's end for an id-less one).
//! - A guessed call whose result then names another subagent retracts the guess with a corrective start without a
//!   task, unless that subagent's task has since been corrected to another call's.
//! - Hooks fired inside a subagent carry its `agent_id` and `agent_type`; its permission requests and questions block
//!   it, and its next tool activity unblocks it.
//! - `SubagentStop { agent_id }` finishes it.
//!
//! So a subagent's task is its own call's, or none, with two exceptions: briefly, a guess its call later retracts;
//! and a subagent whose call fell out of the [`MAX_NAMED`] remembered ones, or completed without an id in an earlier
//! turn, can take a guess from a later call.

use serde::{Deserialize, Serialize};

use crate::acpp::{
    BlockKind, Frame, ShepherdFrame, SubagentBlocked, SubagentFinished, SubagentStarted,
    SubagentWorking,
};

/// How many unclaimed completed spawning calls are remembered; the oldest go first. A subagent whose call was
/// forgotten starts with a guess, or without a task.
pub(crate) const MAX_NAMED: usize = 64;

/// A translator's view of the agent's subagents. It serializes with the translator; a translator carried from a
/// build without it starts with none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Subagents {
    /// Spawning tool calls this turn whose `PostToolUse` hasn't named their subagent yet, oldest first.
    spawns: Vec<Spawn>,
    /// Completed spawning calls whose subagent no `SubagentStart` has claimed and no `SubagentStop` finished, oldest
    /// first, with the id their result named: id-less ones from this turn, named ones from any turn, at most
    /// [`MAX_NAMED`].
    #[serde(default)]
    named: Vec<Named>,
    /// Subagents started this turn, with the task they were announced with.
    #[serde(default)]
    started: Vec<Started>,
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
    id: Option<String>,
    agent_type: String,
    task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Started {
    id: String,
    agent_type: String,
    task: Option<String>,
}

/// How a tool call ended, for [`Subagents::tool_done`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ToolOutcome<'a> {
    /// It failed or was denied: a spawning call spawned nothing.
    Failed,
    /// It completed; a spawning call's result named `spawned` as its subagent's id, if it named one.
    Completed { spawned: Option<&'a str> },
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

    /// Tool call `tool_use_id` ended with `outcome`; the corrective started frames it calls for.
    ///
    /// A failed spawning call spawned nothing and is forgotten. A completed one that named a subagent this turn
    /// already announced gives that subagent its task. One whose guess went to another subagent than the one it
    /// names retracts the guess (a start without a task). Otherwise it is kept as a candidate until a start claims
    /// the id it named: an id-less one for the rest of the turn, a named one across turns (see [`MAX_NAMED`]).
    pub(crate) fn tool_done(&mut self, tool_use_id: &str, outcome: ToolOutcome) -> Vec<Frame> {
        let Some(i) = self
            .spawns
            .iter()
            .position(|s| s.tool_use_id == tool_use_id)
        else {
            return Vec::new();
        };
        let spawn = self.spawns.remove(i);
        let ToolOutcome::Completed { spawned } = outcome else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(guessed) = &spawn.guessed {
            match spawned {
                // The guess stands: its own subagent, or no word otherwise.
                None => return out,
                Some(agent) if agent == guessed => return out,
                Some(_) => {
                    if let Some(wrong) = self.started.iter_mut().find(|s| &s.id == guessed)
                        && wrong.task.is_some()
                        && wrong.task == spawn.task
                    {
                        wrong.task = None;
                        out.push(started_frame(wrong));
                    }
                }
            }
        }
        if let Some(agent) = spawned
            && let Some(started) = self.started.iter_mut().find(|s| s.id == agent)
        {
            if started.task != spawn.task {
                started.task = spawn.task;
                out.push(started_frame(started));
            }
            return out;
        }
        if let Some(agent) = spawned {
            self.named.retain(|n| n.id.as_deref() != Some(agent));
        }
        self.named.push(Named {
            id: spawned.map(str::to_owned),
            agent_type: spawn.agent_type,
            task: spawn.task,
        });
        let excess = self.named.len().saturating_sub(MAX_NAMED);
        self.named.drain(..excess);
        out
    }

    /// `SubagentStart`: the started frame, with the task of the spawning call that named it, or of the one candidate
    /// call of its type.
    pub(crate) fn start(&mut self, id: &str, agent_type: &str) -> Frame {
        let named = self.named.iter().position(|n| n.id.as_deref() == Some(id));
        let task = match (named, self.started.iter().find(|s| s.id == id)) {
            (Some(i), _) => self.named.remove(i).task,
            // Started again: it keeps the task it has, and takes no guess.
            (None, Some(earlier)) => earlier.task.clone(),
            (None, None) => self.guess(id, agent_type),
        };
        self.blocked.retain(|b| b.id != id);
        self.started.retain(|s| s.id != id);
        let started = Started {
            id: id.to_owned(),
            agent_type: agent_type.to_owned(),
            task,
        };
        let frame = started_frame(&started);
        self.started.push(started);
        frame
    }

    /// The task of the only candidate call of `agent_type`, if it is an open spawning call, which is then guessed
    /// for `id`. Candidates are the open, unguessed calls and the completed calls no start has claimed.
    fn guess(&mut self, id: &str, agent_type: &str) -> Option<String> {
        let unclaimed = self
            .named
            .iter()
            .filter(|n| n.agent_type == agent_type)
            .count();
        let mut open = self
            .spawns
            .iter_mut()
            .filter(|s| s.agent_type == agent_type && s.guessed.is_none());
        let only = open.next()?;
        if unclaimed > 0 || open.next().is_some() {
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
        self.named.retain(|n| n.id.as_deref() != Some(id));
        ShepherdFrame::SubagentFinished(SubagentFinished { id: id.to_owned() }).to_frame()
    }

    /// The parent's turn ended: the session fold drops its subagents, so this forgets them too. Ids that completed
    /// calls named are kept: their subagents may start after the turn (Codex's `spawn_agent` returns before the
    /// child's `SubagentStart`).
    pub(crate) fn end_turn(&mut self) {
        self.spawns.clear();
        self.named.retain(|n| n.id.is_some());
        self.started.clear();
        self.blocked.clear();
    }
}

fn started_frame(started: &Started) -> Frame {
    ShepherdFrame::SubagentStarted(SubagentStarted {
        id: started.id.clone(),
        agent_type: started.agent_type.clone(),
        task: started.task.clone(),
    })
    .to_frame()
}
