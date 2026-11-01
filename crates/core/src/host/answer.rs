//! Answering an agent's pending question (T51): the keys the host types for a choice, each agent's own.
//!
//! **Claude Code** (<https://code.claude.com/docs/en/keybindings>, "Confirmation actions" and "Select actions", and
//! <https://code.claude.com/docs/en/interactive-mode>, `Esc`; read 2026-10-31). A permission dialog is in the
//! `Confirmation` context:
//! - `yes`: Enter (`confirm:yes`), which confirms the focused option. A fresh dialog focuses its first, **Yes**
//!   (<https://code.claude.com/docs/en/permissions>, the Bash prompt's options: Yes; Yes, and don't ask again; …; No).
//!   An answer after the user touched the dialog is refused anyway (the host records any typing while it is asked).
//! - `always`: Shift+Tab (`confirm:cycleMode`, `CSI Z`): "selects the option that allows the action for the rest of
//!   the session, when the prompt offers that option".
//! - `no`: Esc (`confirm:no`): "On a permission prompt, `Esc` declines the action, the same as **No** without a
//!   comment".
//!
//! An `AskUserQuestion` question is a list in the `Select` context: option *n* is Down (`select:next`, `CSI B`)
//! *n − 1* times from the first, then Enter (`select:accept`), each its own write; `no` is Esc (`select:cancel`).
//! Number keys aren't used: a bare digit goes to the question's `Other` text row when that has focus ("Text fields").
//!
//! **Codex** (openai/codex rust-v0.166.0, `codex-rs/tui/src/keymap.rs` `RuntimeKeymap::built_in_defaults`, the
//! `ApprovalKeymap` defaults, and `codex-rs/tui/src/bottom_pane/approval_overlay.rs`, whose `try_handle_shortcut`
//! applies an option on its shortcut at once):
//! - `yes`: `y` (`approve`, "Yes, proceed").
//! - `always`: `a` (`approve_for_session`, "Yes, and don't ask again for this command in this session" and the like).
//! - `no`: `n` (`decline`, "No, and tell Codex what to do differently"); its other default, Esc, would interrupt a
//!   turn if the overlay were already gone.
//!
//! Codex has no hook for a question (`agents::codex`), so it is never asked one shepherd could answer. Neither agent is
//! run by the gate: `scripts/e2e-agents.sh` (T44) checks the round trip against the real ones.

use crate::acpp::{AgentAsked, BlockKind, Choice};
use crate::config::AgentKind;

const ENTER: &[u8] = b"\r";
const ESC: &[u8] = b"\x1b";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const DOWN: &[u8] = b"\x1b[B";

/// The writes that answer `asked` with `choice` in `agent`'s TUI, in order; or why `choice` doesn't answer it.
pub fn keys(agent: AgentKind, asked: &AgentAsked, choice: Choice) -> Result<Vec<Vec<u8>>, String> {
    let key = |k: &[u8]| Ok(vec![k.to_vec()]);
    match (agent, &asked.kind, choice) {
        (AgentKind::ClaudeCode, BlockKind::Permission, Choice::Yes) => key(ENTER),
        (AgentKind::ClaudeCode, BlockKind::Permission, Choice::Always) => key(SHIFT_TAB),
        (AgentKind::ClaudeCode, BlockKind::Permission | BlockKind::Question, Choice::No) => {
            key(ESC)
        }
        (AgentKind::ClaudeCode, BlockKind::Question, Choice::Option(n)) => {
            let Some(choices) = &asked.choices else {
                return Err("its question isn't answered by picking one option".to_owned());
            };
            let n = usize::try_from(n.get()).unwrap_or(usize::MAX);
            if n > choices.len() {
                return Err(format!(
                    "its question has {} options, not {n}",
                    choices.len()
                ));
            }
            let mut writes = vec![DOWN.to_vec(); n - 1];
            writes.push(ENTER.to_vec());
            Ok(writes)
        }
        (AgentKind::ClaudeCode, BlockKind::Question, _) => {
            Err("a question takes an option's number, or no".to_owned())
        }
        (AgentKind::Codex, BlockKind::Permission, Choice::Yes) => key(b"y"),
        (AgentKind::Codex, BlockKind::Permission, Choice::Always) => key(b"a"),
        (AgentKind::Codex, BlockKind::Permission, Choice::No) => key(b"n"),
        (_, BlockKind::Permission, Choice::Option(_)) => {
            Err("a permission takes yes, always or no".to_owned())
        }
        _ => Err("shepherd doesn't know how to answer it".to_owned()),
    }
}
