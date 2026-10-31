//! Prompting an agent (T47, decision 0022): the keys the host types to submit a prompt, and what reading a reply asks
//! of the `Transcripts` port.
//!
//! The host types a prompt the way a user pastes one: as one bracketed paste (`ESC [ 200 ~` … `ESC [ 201 ~`, xterm's
//! bracketed paste mode, DECSET 2004), so its newlines are text, then Enter (`\r`) as a separate write to submit it.
//! - Codex enables bracketed paste (openai/codex rust-v0.166.0 `codex-rs/tui/src/tui.rs`: `EnableBracketedPaste`), so
//!   crossterm hands the whole paste to `ChatComposer::handle_paste`; its test `handle_paste_small_inserts_text`
//!   (`codex-rs/tui/src/bottom_pane/chat_composer.rs`) pastes text with `\n` in it and the next `KeyCode::Enter`
//!   submits it whole, and `pasted_crlf_normalizes_newlines_for_elements` shows `\r\n` read as `\n`. Its paste-burst
//!   detection is only for terminals that send a paste as separate key events.
//! - Claude Code (<https://code.claude.com/docs/en/interactive-mode>, "Multiline input", read 2026-10-31): "Paste
//!   mode: Paste directly" enters multi-line text, and Enter sends it. The same page ("Invisible characters in
//!   pasted text") says that when Enter finds invisible characters (tag characters, bidirectional controls, zero-width
//!   spaces) it removes them and sends *nothing*, waiting for a second Enter. So those are never typed: every format
//!   character (Unicode category Cf) is dropped except the joiners ZWJ and ZWNJ, which it keeps.
//!
//! Newlines are typed as `\n`, every `\r\n` and lone `\r` turned into one. Every other control character is dropped
//! (`\t` kept), ESC above all, so nothing in the text can end the paste early and be read as keys. What is typed is
//! what `_shepherd/session/prompted` records. Neither agent is run by the gate: `scripts/e2e-agents.sh` (T44) checks
//! the round trip against the real ones.

use std::path::PathBuf;

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

use crate::config::AgentKind;
use crate::ports::transcripts::TurnMarker;

/// Starts a bracketed paste.
pub const PASTE_START: &[u8] = b"\x1b[200~";
/// Ends a bracketed paste.
pub const PASTE_END: &[u8] = b"\x1b[201~";
/// Submits what was typed.
pub const SUBMIT: &[u8] = b"\r";

const ZWNJ: char = '\u{200C}';
const ZWJ: char = '\u{200D}';

/// `text` as it is typed: newlines as `\n`, without control characters (but `\t`) or format characters (but the
/// joiners). Empty if nothing is left.
pub fn typed(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| match c.general_category() {
            GeneralCategory::Control => c == '\n' || c == '\t',
            GeneralCategory::Format => c == ZWJ || c == ZWNJ,
            _ => true,
        })
        .collect()
}

/// The writes that submit `typed` (already [`typed`]) as one prompt: the paste, then Enter.
pub fn keystrokes(typed: &str) -> [Vec<u8>; 2] {
    let mut paste = PASTE_START.to_vec();
    paste.extend_from_slice(typed.as_bytes());
    paste.extend_from_slice(PASTE_END);
    [paste, SUBMIT.to_vec()]
}

/// A reply to read: everything core knows of where the agent wrote it. The runtime adds the agent's home, as its
/// session was launched (its profile's env in `cwd`), to make the port's `ReplyRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyJob {
    /// Claude Code or Codex.
    pub agent: AgentKind,
    /// Where its hooks said the transcript is, as given.
    pub transcript_path: PathBuf,
    pub since: Option<TurnMarker>,
    /// The profile the session was started with, by name, if recorded.
    pub profile: Option<String>,
    /// Where the agent runs.
    pub cwd: PathBuf,
}
