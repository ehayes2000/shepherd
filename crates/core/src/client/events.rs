//! The status page's events feed (T59): what happened in sessions across every host, newest last
//! ([`Client::events`]).
//!
//! Each log entry a host sends may add events: one for what its frame says (a session made, an agent started, a
//! prompt typed for someone, a PR change, a close) and one for how it moved the session's agent (it asked the user
//! something, the ask was answered, its turn finished, it exited). Asks and answers are read from the session's fold,
//! not from particular frames, so whatever frames make an agent wait on the user (a permission request, a blocked
//! subagent, T51's `_shepherd/agent/asked`) show here as an ask, and whatever ends the wait as its answer.
//!
//! The feed keeps the latest [`EVENTS_KEPT`] events, ordered by their entries' `ts` (hosts' clocks, so a peer's
//! history catching up slots in where it happened). An entry is read once: one sent again (a peer's log replayed after
//! it was dropped and listed again) adds nothing. Event text is the agents' and other clients', raw: display it only
//! through `display_safe`. `Prompted::by` is the asking client's claim, a label only.

use std::collections::VecDeque;

use jiff::SignedDuration;

use super::Client;
use crate::acpp::{Envelope, Frame, Pr, PromptBy, Seq, ShepherdFrame};
use crate::ids::{MachineId, SessionId};
use crate::session::{Activity, SessionState};

/// How many events the feed keeps: the oldest go first.
pub const EVENTS_KEPT: usize = 200;

/// Something that happened in a session.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    /// When: its log entry's `ts`.
    pub at: jiff::Timestamp,
    /// The machine whose host sent it.
    pub machine: MachineId,
    pub session: SessionId,
    /// What the session was called when it happened (it may since have been renamed or closed).
    pub name: String,
    pub kind: EventKind,
    /// Its log entry's seq in the session's log, so an entry sent again isn't a second event.
    seq: Seq,
}

impl FeedEvent {
    /// How long ago it happened, as of the client's latest tick. `None` before the first tick.
    pub fn age(&self, client: &Client) -> Option<SignedDuration> {
        client.now.map(|now| now.duration_since(self.at))
    }
}

/// What happened.
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// The session was made.
    Created,
    /// Its agent started (`_shepherd/agent/started`), naming itself.
    Started { agent: String },
    /// Its agent (or a subagent) started waiting on the user, asking `text` if it said.
    Asked { text: Option<String> },
    /// The wait on the user ended and the agent went back to work.
    Answered,
    /// The host typed a prompt into its agent for `by` (`_shepherd/session/prompted`).
    Prompted { text: String, by: PromptBy },
    /// Its agent's turn ended: it is at its prompt.
    Finished,
    /// Its agent exited.
    Exited { code: Option<i32> },
    /// Its branch's pull request changed, or it has none any more.
    Pr { pr: Option<Pr> },
    /// The session was closed.
    Closed,
}

/// The feed: [`EVENTS_KEPT`] at most, oldest first.
#[derive(Debug, Clone, Default)]
pub(super) struct Events {
    events: VecDeque<FeedEvent>,
}

/// Where a session's agent is, as far as the feed tells moves apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Working,
    Waiting,
    Idle,
    Exited,
}

impl Phase {
    fn of(state: &SessionState) -> Self {
        match state.activity() {
            Activity::Exited { .. } => Self::Exited,
            _ if state.wants_user() => Self::Waiting,
            Activity::Working { .. } => Self::Working,
            Activity::Idle => Self::Idle,
            _ => Self::Starting,
        }
    }
}

/// Where a session's agent was before an entry: what [`Client::events_after`] compares with.
#[derive(Debug, Clone)]
pub(super) struct Before {
    phase: Phase,
    name: Option<String>,
}

impl Client {
    /// The events feed, oldest first.
    pub fn events(&self) -> impl DoubleEndedIterator<Item = &FeedEvent> + ExactSizeIterator {
        self.events.events.iter()
    }

    /// What the feed compares a session's entry with: its agent's phase and its name, before the entry is folded.
    pub(super) fn events_before(&self, state: Option<&SessionState>, id: &SessionId) -> Before {
        Before {
            phase: state.map_or(Phase::Starting, Phase::of),
            name: self.listed_name(id),
        }
    }

    /// Adds the events of entry `env` of a session's log from the host on `machine`, once it is folded and the list
    /// rebuilt: its frame's, and `moved` ([`moved`]).
    pub(super) fn events_after(
        &mut self,
        machine: MachineId,
        env: &Envelope,
        moved: Option<EventKind>,
        before: Before,
    ) {
        let Some(session) = env.session() else {
            return;
        };
        let seen = self
            .events
            .events
            .iter()
            .any(|e| e.seq == env.seq() && &e.session == session && e.machine == machine);
        if seen {
            return;
        }
        let kinds = framed(env).into_iter().chain(moved);
        let name = self
            .listed_name(session)
            .or(before.name)
            .unwrap_or_else(|| session.as_str().to_owned());
        for kind in kinds {
            self.events.push(FeedEvent {
                at: env.ts().instant(),
                machine: machine.clone(),
                session: session.clone(),
                name: name.clone(),
                kind,
                seq: env.seq(),
            });
        }
    }

    fn listed_name(&self, id: &SessionId) -> Option<String> {
        self.sessions
            .iter()
            .find(|s| &s.id == id)
            .map(|s| s.name.clone())
    }
}

impl Events {
    /// Adds `event` where its time puts it (after any at the same time), dropping the oldest past [`EVENTS_KEPT`].
    fn push(&mut self, event: FeedEvent) {
        let at = self.events.partition_point(|e| e.at <= event.at);
        self.events.insert(at, event);
        while self.events.len() > EVENTS_KEPT {
            self.events.pop_front();
        }
    }
}

/// The event `env`'s frame itself says, if any.
fn framed(env: &Envelope) -> Option<EventKind> {
    let Frame::Shepherd(message) = env.frame() else {
        return None;
    };
    Some(match message.body() {
        ShepherdFrame::SessionCreated(_) => EventKind::Created,
        ShepherdFrame::AgentStarted(started) => EventKind::Started {
            agent: started.agent.clone(),
        },
        ShepherdFrame::SessionPrompted(prompted) => EventKind::Prompted {
            text: prompted.text.clone(),
            by: prompted.by.clone(),
        },
        ShepherdFrame::SessionPr(pr) => EventKind::Pr { pr: pr.pr.clone() },
        ShepherdFrame::SessionClosed(_) => EventKind::Closed,
        _ => return None,
    })
}

/// The event of a session's agent moving from where it was `before` an entry to where `state` has it after, if the move
/// is one.
pub(super) fn moved(before: &Before, state: &SessionState) -> Option<EventKind> {
    let before = before.phase;
    let after = Phase::of(state);
    if after == before {
        return None;
    }
    Some(match (before, after) {
        (_, Phase::Waiting) => EventKind::Asked {
            text: state.asked().map(str::to_owned),
        },
        (_, Phase::Exited) => EventKind::Exited {
            code: match state.activity() {
                Activity::Exited { code } => *code,
                _ => None,
            },
        },
        (Phase::Waiting, Phase::Working) => EventKind::Answered,
        (Phase::Working | Phase::Waiting, Phase::Idle) => EventKind::Finished,
        _ => return None,
    })
}
