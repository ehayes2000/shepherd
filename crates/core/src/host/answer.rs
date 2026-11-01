//! Answering an agent's pending question (T51): the keys the host types for a choice, each agent's own, for the dialog
//! the hook said is up (`AgentAsked::tool`). A choice that dialog doesn't offer, or one shepherd can't tell it offers,
//! is refused rather than guessed.
//!
//! **Claude Code** (<https://code.claude.com/docs/en/keybindings>, "Confirmation actions" and "Select actions", and
//! <https://code.claude.com/docs/en/interactive-mode>, `Esc`; read 2026-10-31). A permission dialog is in the
//! `Confirmation` context:
//! - `yes`: Enter (`confirm:yes`), which confirms the focused option. A fresh dialog focuses its first, **Yes**
//!   (<https://code.claude.com/docs/en/permissions>, the Bash prompt's options: Yes; Yes, and don't ask again; …; No).
//!   An answer after the user touched the dialog is refused anyway (the host records any typing while it is asked).
//! - `always`: Shift+Tab (`confirm:cycleMode`, `CSI Z`): "selects the option that allows the action for the rest of
//!   the session, when the prompt offers that option". Only file prompts are documented to offer one (the permissions
//!   page's table: a file modification's "Yes, and don't ask again" lasts "Until session end"; a Bash or WebFetch one
//!   saves a rule instead), so `always` is typed only for `Edit`, `Write`, `MultiEdit` and `NotebookEdit`.
//! - `no`: Esc (`confirm:no`): "On a permission prompt, `Esc` declines the action, the same as **No** without a
//!   comment".
//!
//! An `AskUserQuestion` question is a list in the `Select` context: option *n* is Down (`select:next`, `CSI B`)
//! *n − 1* times from the first, then Enter (`select:accept`), each its own write; `no` is Esc (`select:cancel`).
//! Number keys aren't used: a bare digit goes to the question's `Other` text row when that has focus ("Text fields").
//!
//! **Codex** (openai/codex rust-v0.166.0, `codex-rs/tui/src/keymap.rs` `RuntimeKeymap::built_in_defaults`, the
//! `ApprovalKeymap` defaults, and `codex-rs/tui/src/bottom_pane/approval_overlay.rs`, whose `try_handle_shortcut`
//! applies an option on its shortcut at once). Every approval offers `approve`, `y`, as its first option. The others
//! depend on the request:
//! - `apply_patch` (`patch_options`, fixed): `always` is `a` (`approve_for_session`, "Yes, and don't ask again for
//!   these files"); `no` is `n` (`decline`, "No, and tell Codex what to do differently", its only no).
//! - a command (`Bash`, `exec_options`): the options follow the request's `available_decisions`, which the hook
//!   doesn't carry. `no` is `d` (`deny`, "No, continue without running it"), not `n`, whose Cancel aborts the turn.
//!   `always` (`a`, only with `AcceptForSession` offered) is refused: shepherd can't tell.
//! - anything else (permissions, MCP): only `yes`; their no keys differ (`permissions_options` binds `d` and not `n`).
//!
//! Codex has no hook for a question (`agents::codex`), so it is never asked one shepherd could answer. Neither agent is
//! run by the gate: `scripts/e2e-agents.sh` (T44) checks the round trip, and each variant, against the real ones.

use crate::acpp::{AgentAsked, BlockKind, Choice};
use crate::config::AgentKind;

const ENTER: &[u8] = b"\r";
const ESC: &[u8] = b"\x1b";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const DOWN: &[u8] = b"\x1b[B";

/// Claude Code's file tools, whose prompts offer allowing the edit for the rest of the session.
const CLAUDE_FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// The writes that answer `asked` with `choice` in `agent`'s TUI, in order; or why `choice` doesn't answer it.
pub fn keys(agent: AgentKind, asked: &AgentAsked, choice: Choice) -> Result<Vec<Vec<u8>>, String> {
    let key = |k: &[u8]| Ok(vec![k.to_vec()]);
    let tool = asked.tool.as_deref();
    let not_offered = || {
        Err(format!(
            "its {} dialog doesn't offer {choice}, as far as shepherd can tell",
            tool.unwrap_or("permission")
        ))
    };
    match (agent, &asked.kind, choice) {
        (AgentKind::ClaudeCode, BlockKind::Permission, Choice::Yes) => key(ENTER),
        (AgentKind::ClaudeCode, BlockKind::Permission, Choice::Always) => {
            match tool.is_some_and(|t| CLAUDE_FILE_TOOLS.contains(&t)) {
                true => key(SHIFT_TAB),
                false => not_offered(),
            }
        }
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
        (AgentKind::Codex, BlockKind::Permission, Choice::Always) => match tool {
            Some("apply_patch") => key(b"a"),
            _ => not_offered(),
        },
        (AgentKind::Codex, BlockKind::Permission, Choice::No) => match tool {
            Some("apply_patch") => key(b"n"),
            Some("Bash") => key(b"d"),
            _ => not_offered(),
        },
        (_, BlockKind::Permission, Choice::Option(_)) => {
            Err("a permission takes yes, always or no".to_owned())
        }
        _ => Err("shepherd doesn't know how to answer it".to_owned()),
    }
}
