//! Pure translation of agent hook payloads into ACP++ frames. See "Synthesized frames" in `docs/acpp.md`.

pub mod claude;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::acpp::FrameDraft;
use crate::config::AgentKind;
use crate::ids::SessionId;

use self::claude::ClaudeTranslation;

/// One session's translation state, by agent kind.
#[derive(Debug, Clone, PartialEq)]
enum Translation {
    Claude(ClaudeTranslation),
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
            AgentKind::ClaudeCode => true,
            AgentKind::Codex | AgentKind::Shell => false,
        }
    }

    /// Translates one hook payload from `agent` in `session` into the frames to append. `seed` is called only when
    /// the session has no translator for `agent` yet. An agent kind without a translator gives no frames.
    pub fn translate(
        &mut self,
        session: &SessionId,
        agent: AgentKind,
        payload: &Value,
        seed: impl FnOnce() -> String,
    ) -> Vec<FrameDraft> {
        match agent {
            AgentKind::ClaudeCode => {
                let translation = self.sessions.entry(session.clone()).or_insert_with(|| {
                    Translation::Claude(ClaudeTranslation::new(session.clone(), seed()))
                });
                let Translation::Claude(state) = translation;
                claude::translate(state, payload)
            }
            AgentKind::Codex | AgentKind::Shell => Vec::new(),
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
