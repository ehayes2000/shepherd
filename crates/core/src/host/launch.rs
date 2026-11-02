//! What a new session runs, built from its agent profile: the spawn spec, plus the files the runtime writes into the
//! session's run directory (`~/.shepherd/run/<session>/`) before spawning, and removes when its PTY exits or it closes.
//!
//! Every session gets [`SESSION_VAR`] and [`SOCKET_VAR`], which `shepherd hook` reads. A Claude Code session also
//! gets shepherd's hooks without touching the user's settings files: the hooks go into a generated
//! [`SETTINGS_FILE`] that `claude` loads with `--settings <file>`. Claude Code puts `--settings` above the user,
//! project and local settings files and merges hooks across all of them, so the user's own hooks keep running
//! (<https://code.claude.com/docs/en/settings>, <https://code.claude.com/docs/en/hooks>, read 2026-10-29).
//!
//! A profile with `shepherd_mcp` (the built-in `pm`, see [`crate::config::PM_PROFILE`]) also gets shepherd's own MCP
//! server, `shepherd mcp`, for that session only: a generated [`MCP_FILE`] in the run directory that `claude` loads with
//! `--mcp-config <file>` ("Load MCP servers from JSON files or strings (space-separated)";
//! <https://code.claude.com/docs/en/cli-reference>, read 2026-11-01). Its format is the `mcpServers` object of
//! <https://code.claude.com/docs/en/mcp> (read 2026-11-01). Without `--strict-mcp-config` the user's own MCP servers
//! still load, and shepherd writes nothing under `~/.claude*`. The flag takes several values, so it goes first,
//! right before `--settings`, which ends its list. The server is this very `shepherd` executable by absolute path, and
//! its `env` names the session, so what its tools type is recorded `by: mcp:<session>` (decision 0022) even if Claude
//! Code didn't pass its own environment on.
//!
//! A Codex session gets shepherd's hooks the same way, as one config override: `codex -c hooks=<inline table>`.
//! Codex parses each `-c` value as TOML into its session-flags config layer, which sits above the user's and
//! project's config, and loads hooks from every layer, so the user's own hooks keep running and `~/.codex` is never
//! touched (`codex-rs/utils/cli/src/config_override.rs`, `codex-rs/hooks/src/engine/discovery.rs`, rust-v0.166.0).
//! The same table carries the trust state for exactly those hooks (see [`codex::hooks_config`], decision 0016).
//! Codex runs a hook command with `$SHELL -lc`, so the `shepherd` path is single-quoted.
//!
//! Resuming an exited session ([`plan_resume`]) runs exactly what [`plan`] runs, plus the agent's resume args right
//! after shepherd's own and before the profile's:
//! - Claude Code: `--resume <session id>` ("Resume a specific session by ID or name";
//!   <https://code.claude.com/docs/en/cli-reference>, read 2026-10-30). Without `--fork-session` it keeps the ID.
//! - Codex: the `resume <SESSION_ID>` subcommand. `-c` overrides given before it are root overrides, which
//!   `finalize_resume_interactive` prepends to the resumed session's own, and the args after it are the same
//!   interactive options as plain `codex` (`codex-rs/cli/src/main.rs` `ResumeCommand`, rust-v0.166.0;
//!   <https://learn.chatgpt.com/docs/developer-commands?surface=cli>, read 2026-10-30).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::agents::{claude, codex};
use crate::config::{AgentKind, AgentProfile};
use crate::ids::SessionId;
use crate::ports::pty::{PtySize, SpawnSpec};

/// The session the agent runs in.
pub const SESSION_VAR: &str = "SHEPHERD_SESSION";
/// The host socket `shepherd hook` connects to.
pub const SOCKET_VAR: &str = "SHEPHERD_SOCK";
/// The generated Claude Code settings, in the session's run directory.
pub const SETTINGS_FILE: &str = "settings.json";
/// The generated Claude Code MCP config naming shepherd's MCP server, in the session's run directory, for a profile with
/// `shepherd_mcp`.
pub const MCP_FILE: &str = "mcp.json";
/// What shepherd's MCP server is called in the agent: its tools are `mcp__shepherd__<tool>`.
pub const MCP_SERVER: &str = "shepherd";

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
    /// The `shepherd` path has a character that single quotes don't make literal in every shell Codex might run
    /// hooks with: `'`, or `\`, which fish reads as an escape inside single quotes.
    #[error("{} can't be quoted for the shell", .0.display())]
    NotShellSafe(PathBuf),
    /// Codex's hook override couldn't be built.
    #[error("cannot build the Codex hooks override")]
    CodexHooks,
    /// This kind of agent has no way to resume a session.
    #[error("{} sessions can't be resumed", .0.as_str())]
    CannotResume(AgentKind),
}

/// What `profile` runs in the session `ctx` describes.
pub fn plan(profile: &AgentProfile, ctx: &LaunchContext) -> Result<Launch, LaunchError> {
    build(profile, ctx, Vec::new())
}

/// What `profile` runs to resume the agent's own session `agent_session` in the session `ctx` describes: [`plan`],
/// with the resume args for the profile's kind (see the module docs).
pub fn plan_resume(
    profile: &AgentProfile,
    ctx: &LaunchContext,
    agent_session: &str,
) -> Result<Launch, LaunchError> {
    let resume = match profile.kind {
        AgentKind::ClaudeCode => "--resume",
        AgentKind::Codex => "resume",
        AgentKind::Shell => return Err(LaunchError::CannotResume(profile.kind)),
    };
    build(
        profile,
        ctx,
        vec![resume.to_owned(), agent_session.to_owned()],
    )
}

/// [`plan`], with `extra` between shepherd's own args and the profile's.
fn build(
    profile: &AgentProfile,
    ctx: &LaunchContext,
    extra: Vec<String>,
) -> Result<Launch, LaunchError> {
    let mut env: BTreeMap<String, String> = profile.env.clone();
    env.insert(SESSION_VAR.to_owned(), ctx.session.as_str().to_owned());
    env.insert(SOCKET_VAR.to_owned(), utf8(&ctx.socket)?.to_owned());

    let (mut args, files) = match profile.kind {
        AgentKind::ClaudeCode => {
            let mut args = Vec::new();
            let mut files = Vec::new();
            if profile.shepherd_mcp {
                let mcp = ctx.run_dir.join(MCP_FILE);
                args.extend(["--mcp-config".to_owned(), utf8(&mcp)?.to_owned()]);
                files.push(RunFile {
                    name: MCP_FILE.to_owned(),
                    contents: claude_mcp_config(utf8(&ctx.shepherd)?, &ctx.session),
                });
            }
            let settings = ctx.run_dir.join(SETTINGS_FILE);
            args.extend(["--settings".to_owned(), utf8(&settings)?.to_owned()]);
            files.push(RunFile {
                name: SETTINGS_FILE.to_owned(),
                contents: claude_settings(utf8(&ctx.shepherd)?),
            });
            (args, files)
        }
        AgentKind::Codex => {
            let command = format!(
                "{} hook {}",
                shell_quote(&ctx.shepherd)?,
                AgentKind::Codex.as_str()
            );
            let hooks = codex::hooks_config(&command).ok_or(LaunchError::CodexHooks)?;
            let hooks = toml::Value::Table(hooks);
            (vec!["-c".to_owned(), format!("hooks={hooks}")], Vec::new())
        }
        AgentKind::Shell => (Vec::new(), Vec::new()),
    };
    args.extend(extra);
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

/// An MCP config with one stdio server, [`MCP_SERVER`]: `shepherd mcp`, run by absolute path, told which session it
/// serves.
fn claude_mcp_config(shepherd: &str, session: &SessionId) -> Vec<u8> {
    let config = json!({
        "mcpServers": {
            MCP_SERVER: {
                "type": "stdio",
                "command": shepherd,
                "args": ["mcp"],
                "env": { SESSION_VAR: session.as_str() },
            }
        }
    });
    let mut contents = serde_json::to_vec_pretty(&config)
        .unwrap_or_else(|e| unreachable!("a JSON value always serializes: {e}"));
    contents.push(b'\n');
    contents
}

/// `path` in single quotes, which `sh`, `bash`, `zsh` and `fish` all read literally when it holds no `'` and no `\`.
fn shell_quote(path: &Path) -> Result<String, LaunchError> {
    let text = utf8(path)?;
    if text.contains(['\'', '\\']) {
        return Err(LaunchError::NotShellSafe(path.to_path_buf()));
    }
    Ok(format!("'{text}'"))
}

fn utf8(path: &Path) -> Result<&str, LaunchError> {
    path.to_str()
        .ok_or_else(|| LaunchError::NotUtf8(path.to_path_buf()))
}
