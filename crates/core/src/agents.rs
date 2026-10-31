//! Pure translation of agent hook payloads into ACP++ frames. See "Synthesized frames" in `docs/acpp.md`.

pub mod claude;
pub mod codex;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acpp::FrameDraft;
use crate::config::AgentKind;
use crate::ids::SessionId;

use self::claude::ClaudeTranslation;
use self::codex::CodexTranslation;

/// One session's translation state, by agent kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "agent", content = "state", rename_all = "kebab-case")]
enum Translation {
    Claude(ClaudeTranslation),
    Codex(CodexTranslation),
}

impl Translation {
    fn new(agent: AgentKind, session: &SessionId, seed: String) -> Option<Self> {
        match agent {
            AgentKind::ClaudeCode => {
                Some(Self::Claude(ClaudeTranslation::new(session.clone(), seed)))
            }
            AgentKind::Codex => Some(Self::Codex(CodexTranslation::new(session.clone(), seed))),
            AgentKind::Shell => None,
        }
    }

    fn agent(&self) -> AgentKind {
        match self {
            Self::Claude(_) => AgentKind::ClaudeCode,
            Self::Codex(_) => AgentKind::Codex,
        }
    }
}

/// The translation state of every session that has sent a hook, which the host keeps between `_shepherd/hook/frame`
/// requests so a prompt and its `Stop` pair up.
///
/// It is not host state: it is not a fold over the logs, and a restarted host starts with none. A translator is
/// created on a session's first hook, and again whenever a hook from another agent kind replaces it. Its id seed
/// must be unique among the translators that ever write to that session's log (see [`ClaudeTranslation::new`]), so
/// a new translator never reuses an id an old one left in the log: it is the caller's seed, unique per host run,
/// plus a count of the translators this value has created.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translators {
    sessions: BTreeMap<SessionId, Translation>,
    /// How many translators have been created, which makes each one's seed unique within a host run.
    created: u64,
}

/// [`Translators`] as the host carries them across its exec (decision 0018): each session's translator serialized on
/// its own, so one the new build can't read is dropped without losing the others.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CarriedTranslators {
    sessions: BTreeMap<SessionId, Value>,
    created: u64,
}

impl Translators {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every translator, for the host's exec to carry.
    pub fn carry(&self) -> CarriedTranslators {
        CarriedTranslators {
            sessions: self
                .sessions
                .iter()
                .filter_map(|(id, t)| Some((id.clone(), serde_json::to_value(t).ok()?)))
                .collect(),
            created: self.created,
        }
    }

    /// The translators an earlier image carried, and the sessions whose translator didn't parse and was dropped. A
    /// dropped one is created afresh on the session's next hook, with this host run's seed, so ids stay unique.
    pub fn restore(carried: CarriedTranslators) -> (Self, Vec<SessionId>) {
        let mut dropped = Vec::new();
        let mut sessions = BTreeMap::new();
        for (id, value) in carried.sessions {
            match serde_json::from_value(value) {
                Ok(translation) => {
                    sessions.insert(id, translation);
                }
                Err(_) => dropped.push(id),
            }
        }
        let restored = Self {
            sessions,
            created: carried.created,
        };
        (restored, dropped)
    }

    /// Whether a hook payload from `agent` says its turn ended: its `Stop` hook, for Claude Code and Codex alike (see
    /// the translation tables in [`claude`] and [`codex`]).
    pub fn ends_turn(agent: AgentKind, payload: &Value) -> bool {
        let name = match agent {
            AgentKind::ClaudeCode => claude::hook_name(payload),
            AgentKind::Codex => codex::hook_name(payload),
            AgentKind::Shell => None,
        };
        name == Some("Stop")
    }

    /// Whether hooks from `agent` can be translated.
    pub fn supports(agent: AgentKind) -> bool {
        match agent {
            AgentKind::ClaudeCode | AgentKind::Codex => true,
            AgentKind::Shell => false,
        }
    }

    /// Translates one hook payload from `agent` in `session` into the frames to append. `seed` is called only when
    /// the session has no translator for `agent` yet (a translator for another agent kind is replaced), and must be
    /// unique per host run among the runs that write to the session's log. An agent kind without a translator gives
    /// no frames.
    pub fn translate(
        &mut self,
        session: &SessionId,
        agent: AgentKind,
        payload: &Value,
        seed: impl FnOnce() -> String,
    ) -> Vec<FrameDraft> {
        if self.sessions.get(session).map(Translation::agent) != Some(agent) {
            if !Self::supports(agent) {
                return Vec::new();
            }
            self.created = self.created.saturating_add(1);
            let seed = format!("{}-{}", seed(), self.created);
            let Some(fresh) = Translation::new(agent, session, seed) else {
                return Vec::new();
            };
            self.sessions.insert(session.clone(), fresh);
        }
        match self.sessions.get_mut(session) {
            Some(Translation::Claude(state)) => claude::translate(state, payload),
            Some(Translation::Codex(state)) => codex::translate(state, payload),
            None => Vec::new(),
        }
    }

    /// Drops `session`'s translator, if it has one. The host does this when the session closes.
    pub fn remove(&mut self, session: &SessionId) {
        self.sessions.remove(session);
    }

    /// How many sessions have a translator.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}
