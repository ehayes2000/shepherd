//! What a new session runs, built from its agent profile: the spawn spec, plus the files the runtime writes into the
//! session's run directory (`~/.shepherd/run/<session>/`) before spawning, and removes when the session closes.
//!
//! Every session gets [`SESSION_VAR`] and [`SOCKET_VAR`], which `shepherd hook` reads. A Claude Code session also
//! gets shepherd's hooks without touching the user's settings files: the hooks go into a generated
//! [`SETTINGS_FILE`] that `claude` loads with `--settings <file>`. Claude Code puts `--settings` above the user,
//! project and local settings files and merges hooks across all of them, so the user's own hooks keep running
//! (<https://code.claude.com/docs/en/settings>, <https://code.claude.com/docs/en/hooks>, read 2026-10-29).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::agents::claude;
use crate::config::{AgentKind, AgentProfile};
use crate::ids::SessionId;
use crate::ports::pty::{PtySize, SpawnSpec};

/// The session the agent runs in.
pub const SESSION_VAR: &str = "SHEPHERD_SESSION";
/// The host socket `shepherd hook` connects to.
pub const SOCKET_VAR: &str = "SHEPHERD_SOCK";
/// The generated Claude Code settings, in the session's run directory.
pub const SETTINGS_FILE: &str = "settings.json";

/// What the runtime knows about where a session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchContext {
    pub session: SessionId,
    /// Where the agent starts.
    pub cwd: PathBuf,
    pub size: PtySize,
    /// This session's run directory, absolute. [`Launch::files`] are written into it.
    pub run_dir: PathBuf,
    /// The running `shepherd` executable, absolute, so hooks work without `shepherd` on `PATH`.
    pub shepherd: PathBuf,
    /// The host socket.
    pub socket: PathBuf,
}

/// A file to write into the session's run directory before spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFile {
    /// A plain file name, never a path.
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub spawn: SpawnSpec,
    /// Empty when the agent needs none: then nothing is written and no run directory is made.
    pub files: Vec<RunFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// A path that must go into an argument, an environment variable or the settings JSON isn't UTF-8.
    #[error("{} is not valid UTF-8", .0.display())]
    NotUtf8(PathBuf),
}

/// What `profile` runs in the session `ctx` describes.
pub fn plan(profile: &AgentProfile, ctx: &LaunchContext) -> Result<Launch, LaunchError> {
    let mut env: BTreeMap<String, String> = profile.env.clone();
    env.insert(SESSION_VAR.to_owned(), ctx.session.as_str().to_owned());
    env.insert(SOCKET_VAR.to_owned(), utf8(&ctx.socket)?.to_owned());

    let (mut args, files) = match profile.kind {
        AgentKind::ClaudeCode => {
            let settings = ctx.run_dir.join(SETTINGS_FILE);
            let args = vec!["--settings".to_owned(), utf8(&settings)?.to_owned()];
            let file = RunFile {
                name: SETTINGS_FILE.to_owned(),
                contents: claude_settings(utf8(&ctx.shepherd)?),
            };
            (args, vec![file])
        }
        AgentKind::Codex | AgentKind::Shell => (Vec::new(), Vec::new()),
    };
    args.extend(profile.args.iter().cloned());

    Ok(Launch {
        spawn: SpawnSpec {
            command: profile.command.clone(),
            args,
            cwd: ctx.cwd.clone(),
            env: env.into_iter().collect(),
            size: ctx.size,
        },
        files,
    })
}

/// Settings that run `shepherd hook claude-code` on every hook in [`claude::HOOK_EVENTS`]. Exec form (`args`
/// present): Claude Code spawns `command` directly with `args`, with no shell, so the path needs no quoting. No
/// `matcher`, so each hook fires for every tool and notification type.
fn claude_settings(shepherd: &str) -> Vec<u8> {
    let handler = json!([{
        "hooks": [{
            "type": "command",
            "command": shepherd,
            "args": ["hook", AgentKind::ClaudeCode.as_str()],
        }]
    }]);
    let hooks: Map<String, Value> = claude::HOOK_EVENTS
        .iter()
        .map(|event| ((*event).to_owned(), handler.clone()))
        .collect();
    let mut contents = serde_json::to_vec_pretty(&json!({ "hooks": hooks }))
        .unwrap_or_else(|e| unreachable!("a JSON value always serializes: {e}"));
    contents.push(b'\n');
    contents
}

fn utf8(path: &Path) -> Result<&str, LaunchError> {
    path.to_str()
        .ok_or_else(|| LaunchError::NotUtf8(path.to_path_buf()))
}
