//! Pure translation of agent hook payloads into ACP++ frames. See "Synthesized frames" in `docs/acpp.md`.

pub mod claude;
pub mod codex;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::acpp::FrameDraft;
use crate::config::AgentKind;
use crate::ids::SessionId;

use self::claude::ClaudeTranslation;
use self::codex::CodexTranslation;

/// One session's translation state, by agent kind.
#[derive(Debug, Clone, PartialEq)]
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
/// created on a session's first hook with a seed from the caller, which must be unique among the translators that
/// ever write to that session's log (see [`ClaudeTranslation::new`]), so a new translator never reuses an id an old
/// one left in the log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translators {
    sessions: BTreeMap<SessionId, Translation>,
}

impl Translators {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether hooks from `agent` can be translated.
    pub fn supports(agent: AgentKind) -> bool {
        match agent {
            AgentKind::ClaudeCode | AgentKind::Codex => true,
            AgentKind::Shell => false,
        }
    }

    /// Translates one hook payload from `agent` in `session` into the frames to append. `seed` is called only when
    /// the session has no translator for `agent` yet (a translator for another agent kind is replaced). An agent
    /// kind without a translator gives no frames.
    pub fn translate(
        &mut self,
        session: &SessionId,
        agent: AgentKind,
        payload: &Value,
        seed: impl FnOnce() -> String,
    ) -> Vec<FrameDraft> {
        if self.sessions.get(session).map(Translation::agent) != Some(agent) {
            let Some(fresh) = Translation::new(agent, session, seed()) else {
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
