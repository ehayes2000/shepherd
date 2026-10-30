//! Shepherd's configuration: global defaults, agent profiles, key bindings, notifications and workspace hooks.
//!
//! A [`ConfigSource`](crate::ports::ConfigSource) yields a [`RawConfig`], the file as written, with every field
//! optional. [`Config::resolve`] merges it over the built-in defaults and validates it into a [`Config`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

// ---------- raw (as written) ----------

/// The config file as written. Every field is optional, and unknown fields are rejected so typos surface as
/// errors instead of being silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub defaults: Option<RawDefaults>,
    pub agents: Option<BTreeMap<String, RawAgent>>,
    pub keys: Option<RawKeys>,
    pub notify: Option<RawNotify>,
    /// `[workspaces."<repo path or name>"]`.
    pub workspaces: Option<BTreeMap<String, RawWorkspace>>,
}

/// `[defaults]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDefaults {
    /// The profile new sessions use when none is named.
    pub agent: Option<String>,
    /// Applied to every profile, underneath the profile's own `env`.
    pub env: Option<BTreeMap<String, String>>,
    /// How many days after its PTY exited a session whose branch is merged counts as stale in the cleanup view.
    pub cleanup_after_days: Option<u32>,
}

/// `[agents.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAgent {
    /// `claude-code`, `codex` or `shell`. Kept as text so an unknown kind is a [`ConfigError::UnknownKind`].
    pub kind: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
    /// How long an agent's PTY must stay quiet in a turn before its session is inferred idle. Agent kinds only.
    pub idle_after_quiet_ms: Option<u64>,
}

/// `[keys]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawKeys {
    pub prefix: Option<String>,
}

/// `[notify]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNotify {
    /// Ring the terminal bell (and send an OSC 9 notification) when a session starts wanting the user.
    pub bell: Option<bool>,
}

/// `[workspaces."<repo path or name>"]`: settings for one repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWorkspace {
    pub hooks: Option<RawHooks>,
    /// How long each of its hooks may run, in seconds.
    pub hook_timeout_s: Option<u64>,
}

/// `[workspaces."<repo path or name>".hooks]`: shell commands run in a session's worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHooks {
    /// Run in order in a new worktree, before the agent starts.
    pub post_create: Option<Vec<String>>,
    /// Run in a worktree before it is removed.
    pub pre_remove: Option<Vec<String>>,
}

// ---------- errors ----------

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The source could not be parsed. `line` and `column` are 1-based; `column` counts characters.
    #[error("invalid config at line {line}, column {column}: {message}")]
    Invalid {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("config io: {0}")]
    Io(String),
    #[error("agent `{agent}`: unknown kind `{kind}` (expected claude-code, codex or shell)")]
    UnknownKind { agent: String, kind: String },
    /// A profile that is not built in must say what it is and how to run it.
    #[error("agent `{agent}`: missing `{field}`")]
    MissingField { agent: String, field: &'static str },
    #[error("agent `{agent}`: command is empty")]
    EmptyCommand { agent: String },
    #[error("defaults.agent names unknown profile `{0}`")]
    UnknownDefaultAgent(String),
    #[error("invalid key prefix `{0}` (expected ctrl-<a..z> or ctrl-space)")]
    InvalidKeyPrefix(String),
    #[error("workspace `{0}`: expected an absolute repo path or a repo directory's name")]
    InvalidWorkspaceKey(String),
    #[error("workspace `{workspace}`: a `{list}` hook is empty")]
    EmptyHook {
        workspace: String,
        list: &'static str,
    },
    #[error("workspace `{0}`: hook_timeout_s must be more than 0")]
    ZeroHookTimeout(String),
}

// ---------- resolved ----------

/// On the wire (`_shepherd/hook/frame`) by its [`as_str`](Self::as_str) name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Shell,
}

impl AgentKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "shell" => Some(Self::Shell),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Shell => "shell",
        }
    }
}

/// How to run one kind of agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub name: String,
    pub kind: AgentKind,
    pub command: String,
    pub args: Vec<String>,
    /// Added to the inherited environment. Already includes `[defaults] env`.
    pub env: BTreeMap<String, String>,
    /// How long the PTY of a session running this profile must be quiet during a turn before the host infers the
    /// agent went idle (`idle_after_quiet_ms`, default [`DEFAULT_IDLE_AFTER_QUIET`]). Ignored for shells.
    pub idle_after_quiet: SignedDuration,
}

/// How long an agent's PTY stays quiet during a turn before the host infers it went idle: agent TUIs animate a
/// spinner while they work, so a few seconds of silence means the agent is waiting.
pub const DEFAULT_IDLE_AFTER_QUIET: SignedDuration = SignedDuration::from_millis(3000);

/// The key that starts a shepherd key chord. Written `ctrl-<a..z>` or `ctrl-space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPrefix {
    /// A lowercase ASCII letter, or `' '` for space, pressed with ctrl.
    ctrl: char,
}

impl KeyPrefix {
    pub const DEFAULT: Self = Self { ctrl: 'b' };

    pub fn parse(s: &str) -> Option<Self> {
        let key = s.strip_prefix("ctrl-")?;
        let ctrl = match key {
            "space" => ' ',
            _ => {
                let mut chars = key.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_lowercase() => c,
                    _ => return None,
                }
            }
        };
        Some(Self { ctrl })
    }

    /// The key pressed with ctrl: a lowercase ASCII letter, or `' '`.
    pub fn ctrl_key(self) -> char {
        self.ctrl
    }
}

impl Default for KeyPrefix {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for KeyPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ctrl {
            ' ' => f.write_str("ctrl-space"),
            c => write!(f, "ctrl-{c}"),
        }
    }
}

/// How long a hook may run when its workspace doesn't say.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A workspace's hooks: shell commands run in a session's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceHooks {
    /// Run in order in a new worktree, before the agent starts.
    pub post_create: Vec<String>,
    /// Run in a worktree before it is removed.
    pub pre_remove: Vec<String>,
    /// How long each hook may run.
    pub timeout: Duration,
}

impl Default for WorkspaceHooks {
    fn default() -> Self {
        Self {
            post_create: Vec::new(),
            pre_remove: Vec::new(),
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }
}

/// What a `[workspaces."<key>"]` table applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspaceKey {
    /// The repo at this absolute path.
    Path(PathBuf),
    /// Any repo whose directory has this name.
    Name(String),
}

impl WorkspaceKey {
    /// A key with a `/` is a path, which must be absolute; any other is a directory name.
    fn parse(key: &str) -> Result<Self, ConfigError> {
        let invalid = || ConfigError::InvalidWorkspaceKey(key.to_owned());
        if key.contains('/') {
            let path = Path::new(key);
            return if path.is_absolute() {
                Ok(Self::Path(path.to_path_buf()))
            } else {
                Err(invalid())
            };
        }
        match key {
            "" | "." | ".." => Err(invalid()),
            _ => Ok(Self::Name(key.to_owned())),
        }
    }
}

/// A validated config. Always has at least the built-in profiles (unless overridden), and the default agent is
/// always one of its profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// A copy of the default entry in `profiles`, resolved once so lookup can't fail.
    default_agent: AgentProfile,
    profiles: BTreeMap<String, AgentProfile>,
    key_prefix: KeyPrefix,
    notify_bell: bool,
    cleanup_after_days: u32,
    workspaces: Vec<(WorkspaceKey, WorkspaceHooks)>,
}

/// How many days after its PTY exited a session whose branch is merged counts as stale, when the config doesn't say.
pub const DEFAULT_CLEANUP_AFTER_DAYS: u32 = 7;

/// The built-in default agent.
pub const DEFAULT_AGENT: &str = "claude";

impl Config {
    /// Merges `raw` over the built-in defaults and validates the result. `None` (no config file) gives the
    /// built-in defaults.
    ///
    /// Built-ins: `claude` (claude-code, `claude`), `codex` (codex, `codex`) and `shell` (shell, `login_shell`).
    /// `login_shell` is the user's shell as the runtime found it (e.g. `$SHELL`); when it is `None` or empty the
    /// `shell` profile runs `sh`. A command set in `[agents.shell]` always wins, even `command = "sh"`.
    ///
    /// Merge rules:
    /// - Built-in profiles always exist. A user `[agents.<builtin>]` table overlays the built-in field by field:
    ///   fields it leaves out keep their built-in values.
    /// - `args` replaces the built-in args wholesale when given.
    /// - `env` merges by key: `[defaults] env`, then the built-in profile's env, then `[agents.<name>] env`; later
    ///   wins.
    /// - A profile that is not built in must give `kind` and `command`.
    /// - `[defaults] agent` defaults to `claude` and must name a resulting profile.
    /// - `[keys] prefix` defaults to `ctrl-b` and must be `ctrl-<a..z>` or `ctrl-space`.
    /// - `[notify] bell` defaults to off.
    /// - `[defaults] cleanup_after_days` defaults to [`DEFAULT_CLEANUP_AFTER_DAYS`].
    /// - `idle_after_quiet_ms` defaults to [`DEFAULT_IDLE_AFTER_QUIET`].
    /// - `[workspaces."<key>"]`: `<key>` is an absolute repo path (it has a `/`) or a repo directory's name. Hooks
    ///   must not be empty, and `hook_timeout_s` defaults to [`DEFAULT_HOOK_TIMEOUT`] and must be more than 0.
    pub fn resolve(
        raw: Option<RawConfig>,
        login_shell: Option<String>,
    ) -> Result<Self, ConfigError> {
        let raw = raw.unwrap_or_default();
        let defaults = raw.defaults.unwrap_or_default();
        let default_env = defaults.env.unwrap_or_default();

        let mut profiles: BTreeMap<String, AgentProfile> = builtin_profiles(login_shell)
            .map(|p| (p.name.clone(), p))
            .collect();
        for (name, user) in raw.agents.unwrap_or_default() {
            let profile = match profiles.remove(&name) {
                Some(builtin) => overlay(builtin, user)?,
                None => new_profile(name, user)?,
            };
            profiles.insert(profile.name.clone(), profile);
        }
        for profile in profiles.values_mut() {
            let mut env = default_env.clone();
            env.append(&mut profile.env);
            profile.env = env;
        }

        let cleanup_after_days = defaults
            .cleanup_after_days
            .unwrap_or(DEFAULT_CLEANUP_AFTER_DAYS);
        let default_name = defaults.agent.unwrap_or_else(|| DEFAULT_AGENT.to_owned());
        let default_agent = profiles
            .get(&default_name)
            .cloned()
            .ok_or(ConfigError::UnknownDefaultAgent(default_name))?;

        let key_prefix = match raw.keys.and_then(|k| k.prefix) {
            Some(p) => KeyPrefix::parse(&p).ok_or(ConfigError::InvalidKeyPrefix(p))?,
            None => KeyPrefix::DEFAULT,
        };

        let workspaces = raw
            .workspaces
            .unwrap_or_default()
            .into_iter()
            .map(|(key, workspace)| workspace_hooks(key, workspace))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            default_agent,
            profiles,
            key_prefix,
            notify_bell: raw.notify.and_then(|n| n.bell).unwrap_or(false),
            cleanup_after_days,
            workspaces,
        })
    }

    /// The profile used when none is named.
    pub fn default_agent(&self) -> &AgentProfile {
        &self.default_agent
    }

    pub fn profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles.get(name)
    }

    /// The profile called `name`, or the default one when `name` is `None`. `None` when no profile has that name.
    pub fn profile_or_default(&self, name: Option<&str>) -> Option<&AgentProfile> {
        match name {
            Some(name) => self.profile(name),
            None => Some(&self.default_agent),
        }
    }

    /// All profiles, by name.
    pub fn profiles(&self) -> impl Iterator<Item = &AgentProfile> {
        self.profiles.values()
    }

    pub fn key_prefix(&self) -> KeyPrefix {
        self.key_prefix
    }

    /// Whether a client rings the bell when a session starts wanting the user (`[notify] bell`).
    pub fn notify_bell(&self) -> bool {
        self.notify_bell
    }

    /// How many days after its PTY exited a session whose branch is merged counts as stale in the cleanup view
    /// (`[defaults] cleanup_after_days`).
    pub fn cleanup_after_days(&self) -> u32 {
        self.cleanup_after_days
    }

    /// The hooks of the workspace whose repo is at `repo` (as resolved, symlinks followed): the table whose key is
    /// that path, compared as a path, else the one whose key is its directory's name, else none.
    pub fn workspace_hooks(&self, repo: &Path) -> WorkspaceHooks {
        let by_path = self.workspaces.iter().find(|(key, _)| match key {
            WorkspaceKey::Path(path) => path == repo,
            WorkspaceKey::Name(_) => false,
        });
        let by_name = || {
            self.workspaces.iter().find(|(key, _)| match key {
                WorkspaceKey::Name(name) => repo.file_name().is_some_and(|n| n == name.as_str()),
                WorkspaceKey::Path(_) => false,
            })
        };
        by_path
            .or_else(by_name)
            .map(|(_, hooks)| hooks.clone())
            .unwrap_or_default()
    }
}

fn workspace_hooks(
    key: String,
    workspace: RawWorkspace,
) -> Result<(WorkspaceKey, WorkspaceHooks), ConfigError> {
    let parsed = WorkspaceKey::parse(&key)?;
    let timeout = match workspace.hook_timeout_s {
        Some(0) => return Err(ConfigError::ZeroHookTimeout(key)),
        Some(secs) => Duration::from_secs(secs),
        None => DEFAULT_HOOK_TIMEOUT,
    };
    let raw = workspace.hooks.unwrap_or_default();
    let list = |hooks: Option<Vec<String>>, list| {
        let hooks = hooks.unwrap_or_default();
        match hooks.iter().any(|h| h.trim().is_empty()) {
            true => Err(ConfigError::EmptyHook {
                workspace: key.clone(),
                list,
            }),
            false => Ok(hooks),
        }
    };
    let hooks = WorkspaceHooks {
        post_create: list(raw.post_create, "post_create")?,
        pre_remove: list(raw.pre_remove, "pre_remove")?,
        timeout,
    };
    Ok((parsed, hooks))
}

fn builtin_profiles(login_shell: Option<String>) -> impl Iterator<Item = AgentProfile> {
    let shell = login_shell
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "sh".to_owned());
    [
        ("claude", AgentKind::ClaudeCode, "claude".to_owned()),
        ("codex", AgentKind::Codex, "codex".to_owned()),
        ("shell", AgentKind::Shell, shell),
    ]
    .into_iter()
    .map(|(name, kind, command)| AgentProfile {
        name: name.to_owned(),
        kind,
        command,
        args: Vec::new(),
        env: BTreeMap::new(),
        idle_after_quiet: DEFAULT_IDLE_AFTER_QUIET,
    })
}

/// A user table over a built-in profile: given fields replace, `env` merges by key.
fn overlay(mut profile: AgentProfile, user: RawAgent) -> Result<AgentProfile, ConfigError> {
    if let Some(kind) = user.kind {
        profile.kind = parse_kind(&profile.name, kind)?;
    }
    if let Some(command) = user.command {
        profile.command = command;
    }
    if let Some(args) = user.args {
        profile.args = args;
    }
    profile.env.extend(user.env.unwrap_or_default());
    if let Some(ms) = user.idle_after_quiet_ms {
        profile.idle_after_quiet = quiet_period(ms);
    }
    check_command(profile)
}

fn new_profile(name: String, user: RawAgent) -> Result<AgentProfile, ConfigError> {
    let missing = |field| ConfigError::MissingField {
        agent: name.clone(),
        field,
    };
    let kind = user.kind.ok_or_else(|| missing("kind"))?;
    let kind = parse_kind(&name, kind)?;
    let command = user.command.ok_or_else(|| missing("command"))?;
    check_command(AgentProfile {
        name,
        kind,
        command,
        args: user.args.unwrap_or_default(),
        env: user.env.unwrap_or_default(),
        idle_after_quiet: user
            .idle_after_quiet_ms
            .map_or(DEFAULT_IDLE_AFTER_QUIET, quiet_period),
    })
}

/// `idle_after_quiet_ms` as a duration. Values past what a duration holds saturate, which means never.
fn quiet_period(ms: u64) -> SignedDuration {
    SignedDuration::from_millis(i64::try_from(ms).unwrap_or(i64::MAX))
}

fn parse_kind(agent: &str, kind: String) -> Result<AgentKind, ConfigError> {
    AgentKind::parse(&kind).ok_or_else(|| ConfigError::UnknownKind {
        agent: agent.to_owned(),
        kind,
    })
}

fn check_command(profile: AgentProfile) -> Result<AgentProfile, ConfigError> {
    if profile.command.trim().is_empty() {
        return Err(ConfigError::EmptyCommand {
            agent: profile.name,
        });
    }
    Ok(profile)
}
