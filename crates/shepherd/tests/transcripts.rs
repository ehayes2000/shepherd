//! The `Transcripts` contract on the real reader (`shepherd-archive`, through the adapter in this crate), in a tempdir:
//! both agent homes are temp directories, never the real `~/.claude` or `~/.codex`. The fixture writes each reply the
//! way the agent's transcript holds it (see `crates/archive/src/reply.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::json;
use shepherd::archive::ShepherdTranscripts;
use shepherd_core::config::AgentKind;
use shepherd_core::ports::Reply;
use shepherd_testkit::contracts::transcripts::TranscriptsFixture;
use tempfile::TempDir;

struct RealFixture {
    dir: TempDir,
    port: ShepherdTranscripts,
}

impl RealFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for home in ["claude", "codex"] {
            std::fs::create_dir(dir.path().join(home)).unwrap();
        }
        std::fs::write(dir.path().join("outside.jsonl"), b"{}\n").unwrap();
        Self {
            dir,
            port: ShepherdTranscripts,
        }
    }
}

/// One transcript line holding `reply`, as `agent` writes its assistant messages, with a user line before it.
fn lines(agent: AgentKind, n: usize, reply: &Reply) -> Vec<serde_json::Value> {
    let at = reply.at.map(|t| t.to_string());
    match agent {
        AgentKind::Codex => vec![
            json!({ "timestamp": at, "type": "response_item",
                    "payload": { "type": "message", "role": "user",
                                 "content": [{ "type": "input_text", "text": "go" }] } }),
            json!({ "timestamp": at, "type": "response_item",
                    "payload": { "type": "message", "role": "assistant",
                                 "content": [{ "type": "output_text", "text": reply.text }] } }),
        ],
        _ => vec![
            json!({ "type": "user", "isSidechain": false, "timestamp": at,
                    "message": { "role": "user", "content": "go" } }),
            json!({ "type": "assistant", "isSidechain": false, "timestamp": at,
                    "message": { "id": format!("msg_{n}"), "role": "assistant",
                                 "content": [{ "type": "text", "text": reply.text }] } }),
        ],
    }
}

impl TranscriptsFixture for RealFixture {
    type Port = ShepherdTranscripts;

    fn port(&self) -> &ShepherdTranscripts {
        &self.port
    }

    fn agent_home(&self, agent: AgentKind) -> PathBuf {
        match agent {
            AgentKind::Codex => self.dir.path().join("codex"),
            _ => self.dir.path().join("claude"),
        }
    }

    /// Claude Code: `projects/<project>/<id>.jsonl`. Codex: `sessions/2026/10/31/rollout-<time>-<id>.jsonl`.
    fn write_replies(&self, agent: AgentKind, id: &str, replies: &[Reply]) -> PathBuf {
        let home = self.agent_home(agent);
        let path = match agent {
            AgentKind::Codex => home
                .join("sessions/2026/10/31")
                .join(format!("rollout-2026-10-31T09-00-00-{id}.jsonl")),
            _ => home.join("projects/-wt-app").join(format!("{id}.jsonl")),
        };
        let mut content = String::new();
        for (n, reply) in replies.iter().enumerate() {
            for line in lines(agent, n, reply) {
                content.push_str(&format!("{line}\n"));
            }
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn outside_file(&self) -> PathBuf {
        self.dir.path().join("outside.jsonl")
    }
}

shepherd_testkit::transcripts_contract!(RealFixture::new());
