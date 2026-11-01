//! An agent's last reply, read from the transcript its hooks named (T47): what `shepherd prompt --wait` prints. The
//! transcript is read through the same path checks as a capture ([`crate::capture`]), so a reply is only ever read from
//! the agent's own place for transcripts under its home.
//!
//! The reply is the agent's last assistant message that has any text, its text blocks joined by newlines; only a
//! message written at or after `since` counts. Lines that aren't JSON (one still being written) are skipped. Formats
//! (provisional, from the fixtures' sources in `tests/fixtures/README.md`; T44 re-records them from the real agents):
//! - Claude Code: one JSON object per line. An assistant message is a line with `type: "assistant"` whose `message`
//!   has `role: "assistant"` and `content` blocks; Claude Code writes one line per content block, each carrying the
//!   message's `message.id`, so consecutive lines with the same id are one message. Text blocks are
//!   `{ type: "text", text }`; `thinking` and `tool_use` blocks are not the reply. Lines with `isSidechain: true` are a
//!   subagent's, not the agent's. Each line has a `timestamp` (RFC 3339).
//! - Codex: each rollout line is `{ timestamp, type, payload }`. An assistant message is a `response_item` whose
//!   payload is `{ type: "message", role: "assistant", content }`, with `{ type: "output_text", text }` blocks. The
//!   `event_msg` lines repeating it (`agent_message`, `task_complete`) are not read.

use std::path::Path;

use jiff::Timestamp;
use serde_json::Value;

use crate::capture::{CaptureError, CaptureLimits, read_main_transcript};
use crate::ids::AgentKind;

/// Which reply to read: [`last_reply`].
#[derive(Debug, Clone)]
pub struct ReplyQuery<'a> {
    pub agent: AgentKind,
    /// The agent's home: Claude Code's config dir, or Codex's `CODEX_HOME`. The caller resolves it.
    pub agent_home: &'a Path,
    /// The `transcript_path` of the agent's hook payload, as given.
    pub transcript_path: &'a Path,
    /// Only a message written at or after this counts.
    pub since: Option<Timestamp>,
    /// How much may be read (only `max_bytes` matters: one file is read).
    pub limits: CaptureLimits,
}

/// An agent's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// When it was written: its last line's `timestamp`, if it has one.
    pub at: Option<Timestamp>,
}

/// The agent's last reply in the transcript `query` names, or `None` if it has written none (since `since`).
pub fn last_reply(query: &ReplyQuery<'_>) -> Result<Option<Reply>, CaptureError> {
    let content = read_main_transcript(
        query.agent,
        query.agent_home,
        query.transcript_path,
        query.limits,
    )?;
    Ok(parse(query.agent, &content, query.since))
}

/// One assistant message as read so far: its id (Claude Code's `message.id`), text blocks and last timestamp.
struct Message {
    id: Option<String>,
    texts: Vec<String>,
    at: Option<Timestamp>,
}

fn parse(agent: AgentKind, content: &[u8], since: Option<Timestamp>) -> Option<Reply> {
    let mut last: Option<Message> = None;
    for line in content.split(|&b| b == b'\n') {
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| t.parse::<Timestamp>().ok());
        if let Some(since) = since
            && at.is_none_or(|at| at < since)
        {
            continue;
        }
        let Some((id, texts)) = assistant_texts(agent, &entry) else {
            continue;
        };
        if texts.is_empty() {
            continue;
        }
        match &mut last {
            Some(message) if id.is_some() && message.id == id => {
                message.texts.extend(texts);
                message.at = at.or(message.at);
            }
            _ => last = Some(Message { id, texts, at }),
        }
    }
    last.map(|m| Reply {
        text: m.texts.join("\n"),
        at: m.at,
    })
}

/// If `entry` is (part of) one of the agent's own assistant messages: its id, if it has one, and its text blocks.
fn assistant_texts(agent: AgentKind, entry: &Value) -> Option<(Option<String>, Vec<String>)> {
    let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    match agent {
        AgentKind::Claude => {
            if str_of(entry, "type").as_deref() != Some("assistant")
                || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
            {
                return None;
            }
            let message = entry.get("message")?;
            if str_of(message, "role").as_deref() != Some("assistant") {
                return None;
            }
            let texts = match message.get("content")? {
                Value::String(text) => vec![text.clone()],
                content => blocks(content, "text"),
            };
            Some((str_of(message, "id"), texts))
        }
        AgentKind::Codex => {
            if str_of(entry, "type").as_deref() != Some("response_item") {
                return None;
            }
            let payload = entry.get("payload")?;
            if str_of(payload, "type").as_deref() != Some("message")
                || str_of(payload, "role").as_deref() != Some("assistant")
            {
                return None;
            }
            Some((None, blocks(payload.get("content")?, "output_text")))
        }
    }
}

/// The `text` of each block in `content` (an array) whose `type` is `kind`, skipping empty ones.
fn blocks(content: &Value, kind: &str) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}
