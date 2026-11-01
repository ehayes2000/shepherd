//! The `Transcripts` contract.
//!
//! Each case gets a fresh [`TranscriptsFixture`]: a home for each agent where the fixture writes a transcript holding
//! the replies a case asks for, in that agent's own format and place (Claude Code:
//! `projects/<project>/<session>.jsonl`; Codex: `sessions/YYYY/MM/DD/rollout-<time>-<session>.jsonl`).
//!
//! Run it with [`transcripts_contract!`](crate::transcripts_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Transcripts` contract case. `$fixture` is an expression building a fresh
/// [`TranscriptsFixture`] for each test. New cases go in the list below.
#[macro_export]
macro_rules! transcripts_contract {
    ($fixture:expr) => {
        $crate::transcripts_contract!(@cases $fixture;
            the_last_reply_is_read_for_either_agent,
            only_a_reply_written_since_the_turn_began_counts,
            a_transcript_with_no_reply_yet_has_none,
            a_transcript_outside_the_agent_home_is_refused,
            a_transcript_that_is_not_there_is_missing,
            a_shell_keeps_no_transcript,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::transcripts::$case(&$fixture);
            }
        )*
    };
}

use std::path::PathBuf;

use shepherd_core::acpp::Timestamp;
use shepherd_core::config::AgentKind;
use shepherd_core::ports::{Reply, ReplyRequest, TranscriptError, Transcripts, TurnMarker};

/// A home for each agent, where transcripts can be written.
pub trait TranscriptsFixture {
    type Port: Transcripts;

    fn port(&self) -> &Self::Port;
    /// `agent`'s home: an existing directory.
    fn agent_home(&self, agent: AgentKind) -> PathBuf;
    /// Writes a transcript of `agent`'s session `id` where `agent` keeps it, in its format, holding `replies` (each
    /// the agent's whole assistant message, oldest first, each with a time), and returns its path.
    fn write_replies(&self, agent: AgentKind, id: &str, replies: &[Reply]) -> PathBuf;
    /// An existing file outside every agent home.
    fn outside_file(&self) -> PathBuf;
}

const AGENT_SESSION: &str = "0a1b2c3d-4e5f-4061-8728-394a5b6c7d8e";

/// The marker of a turn that began at `text`.
fn since(text: &str) -> TurnMarker {
    TurnMarker {
        at: Timestamp::parse(text).unwrap().instant(),
    }
}

fn reply(text: &str, when: &str) -> Reply {
    Reply {
        text: text.to_owned(),
        at: Some(since(when).at),
    }
}

fn ask<F: TranscriptsFixture>(
    f: &F,
    agent: AgentKind,
    path: PathBuf,
    from: Option<&str>,
) -> Result<Option<Reply>, TranscriptError> {
    f.port().last_reply(&ReplyRequest {
        agent,
        agent_home: f.agent_home(agent),
        transcript_path: path,
        since: from.map(since),
    })
}

pub fn the_last_reply_is_read_for_either_agent<F: TranscriptsFixture>(f: &F) {
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let replies = [
            reply("first", "2026-10-31T10:00:01Z"),
            reply("second,\non two lines", "2026-10-31T10:00:02Z"),
        ];
        let path = f.write_replies(agent, AGENT_SESSION, &replies);
        assert_eq!(
            ask(f, agent, path, None),
            Ok(Some(replies[1].clone())),
            "{agent:?}"
        );
    }
}

pub fn only_a_reply_written_since_the_turn_began_counts<F: TranscriptsFixture>(f: &F) {
    let replies = [
        reply("old", "2026-10-31T10:00:01Z"),
        reply("new", "2026-10-31T10:05:01Z"),
    ];
    let path = f.write_replies(AgentKind::ClaudeCode, AGENT_SESSION, &replies);
    let got = |from| ask(f, AgentKind::ClaudeCode, path.clone(), Some(from));
    assert_eq!(got("2026-10-31T10:05:00Z"), Ok(Some(replies[1].clone())));
    assert_eq!(got("2026-10-31T10:05:01Z"), Ok(Some(replies[1].clone())));
    assert_eq!(got("2026-10-31T10:06:00Z"), Ok(None));
}

pub fn a_transcript_with_no_reply_yet_has_none<F: TranscriptsFixture>(f: &F) {
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let path = f.write_replies(agent, AGENT_SESSION, &[]);
        assert_eq!(ask(f, agent, path, None), Ok(None), "{agent:?}");
    }
}

pub fn a_transcript_outside_the_agent_home_is_refused<F: TranscriptsFixture>(f: &F) {
    let outside = f.outside_file();
    assert!(
        matches!(
            ask(f, AgentKind::ClaudeCode, outside, None),
            Err(TranscriptError::Refused(_))
        ),
        "outside"
    );
}

pub fn a_transcript_that_is_not_there_is_missing<F: TranscriptsFixture>(f: &F) {
    let path = f.write_replies(AgentKind::ClaudeCode, AGENT_SESSION, &[]);
    let gone = path.with_file_name("0f0f0f0f-0000-4000-8000-000000000000.jsonl");
    assert!(
        matches!(
            ask(f, AgentKind::ClaudeCode, gone, None),
            Err(TranscriptError::Missing(_))
        ),
        "missing"
    );
}

pub fn a_shell_keeps_no_transcript<F: TranscriptsFixture>(f: &F) {
    let path = f.write_replies(AgentKind::ClaudeCode, AGENT_SESSION, &[]);
    let home = f.agent_home(AgentKind::ClaudeCode);
    let asked = f.port().last_reply(&ReplyRequest {
        agent: AgentKind::Shell,
        agent_home: home,
        transcript_path: path,
        since: None,
    });
    assert!(
        matches!(asked, Err(TranscriptError::Refused(_))),
        "{asked:?}"
    );
}
