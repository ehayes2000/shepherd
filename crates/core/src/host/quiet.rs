//! The quiet-output idle fallback: an agent session whose PTY goes quiet during a turn is inferred idle.
//!
//! Some ways a turn ends fire no hook: Esc interrupts a turn without a `Stop`, another `UserPromptSubmit` hook can
//! block a prompt, and choosing "No" in Claude Code's permission dialog fires nothing. Agent TUIs animate a spinner
//! while they work, so a PTY that stays quiet for the profile's `idle_after_quiet` means the agent is waiting. Then
//! the host appends `_shepherd/agent/idle` with how it inferred it, which sets `waiting_since` like a real idle.
//!
//! - **Working**: quiet for the period since the last PTY output or log entry gives
//!   [`IdleInference::Quiet`].
//! - **NeedsPermission**: an untouched dialog is legitimately quiet, so quiet alone never clears it, and neither does
//!   output alone (the dialog's own first draw and resize redraws come after the request too). An answer written to
//!   the PTY after the request, then output (it redrew), then the quiet period gives
//!   [`IdleInference::QuietAfterPermission`]. An answer is client input that could pick an option: Enter, a printable
//!   key or a paste. Moving around the dialog, focus and mouse reports, and the screen's own query replies (which the
//!   runtime writes without telling the watch) are not answers.
//! - Anything else, and shell sessions always, are left alone. Output never makes a session working again: only a
//!   real turn signal does (`core::session`).
//! - A tick that asks for an idle restarts that session's quiet period, so an idle the runtime failed to append is
//!   asked for again at most once per period.
//!
//! Not host state: PTY output isn't logged, so when it last happened is known only to this host run. The runtime keeps
//! one [`QuietWatch`], applies every entry it appends (and replays the logs into it on start), feeds it PTY output,
//! client input and ticks with the time they happened, wakes up at its [`QuietWatch::deadline`], and executes what it returns like
//! [`Host::handle`](super::Host::handle)'s commands.

use std::collections::BTreeMap;

use jiff::SignedDuration;

use super::{HostCommand, append};
use crate::acpp::acp::AcpFrame;
use crate::acpp::{AgentIdle, Envelope, Frame, IdleInference, ShepherdFrame, Timestamp};
use crate::config::{AgentKind, Config, DEFAULT_IDLE_AFTER_QUIET};
use crate::ids::SessionId;
use crate::session::{Activity, SessionState};
use crate::wire::InputKind;

/// Time passing, for a [`QuietWatch`]. The runtime stamps each with the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuietEvent {
    /// `session`'s PTY printed something at `at`.
    Output { session: SessionId, at: Timestamp },
    /// A client's input of `kind` was written to `session`'s PTY at `at`.
    Input {
        session: SessionId,
        at: Timestamp,
        kind: InputKind,
        bytes: Vec<u8>,
    },
    /// It is `at`: the deadline the runtime scheduled, or later.
    Tick { at: Timestamp },
}

/// Watches agent sessions for a quiet PTY during a turn. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct QuietWatch {
    /// The quiet period of every agent profile in the config, by name.
    profiles: BTreeMap<String, SignedDuration>,
    /// Open agent sessions.
    sessions: BTreeMap<SessionId, Watched>,
}

#[derive(Debug, Clone, PartialEq)]
struct Watched {
    quiet: SignedDuration,
    state: SessionState,
    /// The latest PTY output or log entry.
    last: Option<jiff::Timestamp>,
    /// How far the current permission request has been answered.
    answer: Answer,
}

/// Evidence that the current permission dialog was answered, though no signal said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// No client input since the request.
    Untouched,
    /// Client input since the request, and no output since.
    Typed,
    /// Client input since the request, then output: it redrew.
    Redrawn,
}

impl Watched {
    /// What it would be inferred as, and from when, if it stayed quiet.
    fn due(&self) -> Option<(jiff::Timestamp, IdleInference)> {
        let inference = match self.state.activity() {
            Activity::Working { .. } => IdleInference::Quiet,
            Activity::NeedsPermission { .. } if self.answer == Answer::Redrawn => {
                IdleInference::QuietAfterPermission
            }
            _ => return None,
        };
        let at = self.last?.checked_add(self.quiet).ok()?;
        Some((at, inference))
    }
}

/// Whether client input of `kind` could answer a dialog: Enter or a printable key, or a paste. Moving around a
/// dialog (cursor keys, Tab, Backspace) doesn't, and neither do the terminal's focus and mouse reports.
///
/// Keys starting with ESC are escape sequences, never text: cursor and function keys, alt chords, and every key the
/// kitty keyboard protocol encodes as `CSI … u`. So Esc and Ctrl-C, which can dismiss a dialog, don't count, and
/// with kitty's "report all keys" flag on no key does. The dialog then waits for a real signal.
fn answers(kind: InputKind, bytes: &[u8]) -> bool {
    match kind {
        InputKind::Paste => true,
        InputKind::Keys if bytes.first() != Some(&0x1b) => String::from_utf8_lossy(bytes)
            .chars()
            .any(|c| c == '\r' || !c.is_control()),
        _ => false,
    }
}

impl QuietWatch {
    /// A watch with the quiet periods of `config`'s profiles.
    pub fn new(config: &Config) -> Self {
        Self {
            profiles: config
                .profiles()
                .map(|p| (p.name.clone(), p.idle_after_quiet))
                .collect(),
            sessions: BTreeMap::new(),
        }
    }

    /// Folds one log entry. A `session/created` for an agent (Claude Code or Codex) starts watching it, with its
    /// profile's quiet period (the default if the profile is gone); `session/closed` stops. Every entry of a watched
    /// session restarts its quiet period.
    pub fn apply(&mut self, env: &Envelope) {
        let Some(id) = env.session() else {
            return;
        };
        if let Frame::Shepherd(m) = env.frame() {
            match m.body() {
                ShepherdFrame::SessionCreated(created) => {
                    let agent = created.agent.as_deref().and_then(AgentKind::parse);
                    if matches!(agent, Some(AgentKind::ClaudeCode | AgentKind::Codex)) {
                        let quiet = created
                            .profile
                            .as_ref()
                            .and_then(|p| self.profiles.get(p))
                            .copied()
                            .unwrap_or(DEFAULT_IDLE_AFTER_QUIET);
                        self.sessions.entry(id.clone()).or_insert(Watched {
                            quiet,
                            state: SessionState::default(),
                            last: None,
                            answer: Answer::Untouched,
                        });
                    }
                }
                ShepherdFrame::SessionClosed(_) => {
                    self.sessions.remove(id);
                    return;
                }
                _ => {}
            }
        }
        let Some(watched) = self.sessions.get_mut(id) else {
            return;
        };
        let was_asking = matches!(watched.state.activity(), Activity::NeedsPermission { .. });
        watched.state.apply(env);
        let asks = matches!(watched.state.activity(), Activity::NeedsPermission { .. });
        let new_request = matches!(env.frame(), Frame::Acp(m) if matches!(m.body(), AcpFrame::RequestPermission(_)));
        if asks && (!was_asking || new_request) {
            watched.answer = Answer::Untouched;
        }
        watched.last = watched.last.max(Some(env.ts().instant()));
    }

    /// Notes PTY output or client input, or appends an idle for every watched session whose quiet period has run out.
    pub fn handle(&mut self, event: QuietEvent) -> Vec<HostCommand> {
        match event {
            QuietEvent::Output { session, at } => {
                if let Some(watched) = self.sessions.get_mut(&session) {
                    if watched.answer == Answer::Typed {
                        watched.answer = Answer::Redrawn;
                    }
                    watched.last = watched.last.max(Some(at.instant()));
                }
                Vec::new()
            }
            QuietEvent::Input {
                session,
                at,
                kind,
                bytes,
            } => {
                if let Some(watched) = self.sessions.get_mut(&session) {
                    let asking =
                        matches!(watched.state.activity(), Activity::NeedsPermission { .. });
                    if asking && watched.answer == Answer::Untouched && answers(kind, &bytes) {
                        watched.answer = Answer::Typed;
                    }
                    watched.last = watched.last.max(Some(at.instant()));
                }
                Vec::new()
            }
            QuietEvent::Tick { at } => self
                .sessions
                .iter_mut()
                .filter_map(|(id, watched)| {
                    let (due, inference) = watched.due()?;
                    (at.instant() >= due).then(|| {
                        // Asked for once per period, even if the append fails.
                        watched.last = Some(at.instant());
                        let idle = AgentIdle {
                            inferred: Some(inference),
                        };
                        append(id, ShepherdFrame::AgentIdle(idle))
                    })
                })
                .collect(),
        }
    }

    /// The earliest time a watched session's quiet period runs out, if any is counting: when to send the next
    /// [`QuietEvent::Tick`].
    pub fn deadline(&self) -> Option<jiff::Timestamp> {
        self.sessions
            .values()
            .filter_map(|w| w.due().map(|(at, _)| at))
            .min()
    }

    /// Whether `session` is watched, so its output matters.
    pub fn watches(&self, session: &SessionId) -> bool {
        self.sessions.contains_key(session)
    }
}
