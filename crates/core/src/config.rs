//! Shepherd's configuration: global defaults, agent profiles and key bindings.
//!
//! A [`ConfigSource`](crate::ports::ConfigSource) yields a [`RawConfig`], the file as written, with every field
//! optional. [`Config::resolve`] merges it over the built-in defaults and validates it into a [`Config`].

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

// ---------- raw (as written) ----------

/// The config file as written. Every field is optional, and unknown fields are rejected so typos surface as
/// errors instead of being silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub defaults: Option<RawDefaults>,
    pub agents: Option<BTreeMap<String, RawAgent>>,
    pub keys: Option<RawKeys>,
}

/// `[defaults]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDefaults {
    /// The profile new sessions use when none is named.
    pub agent: Option<String>,
    /// Applied to every profile, underneath the profile's own `env`.
    pub env: Option<BTreeMap<String, String>>,
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
}

/// `[keys]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawKeys {
    pub prefix: Option<String>,
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
}

// ---------- resolved ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

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

/// A validated config. Always has at least the built-in profiles (unless overridden), and the default agent is
/// always one of its profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// A copy of the default entry in `profiles`, resolved once so lookup can't fail.
    default_agent: AgentProfile,
    profiles: BTreeMap<String, AgentProfile>,
    key_prefix: KeyPrefix,
}

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

        let default_name = defaults.agent.unwrap_or_else(|| DEFAULT_AGENT.to_owned());
        let default_agent = profiles
            .get(&default_name)
            .cloned()
            .ok_or(ConfigError::UnknownDefaultAgent(default_name))?;

        let key_prefix = match raw.keys.and_then(|k| k.prefix) {
            Some(p) => KeyPrefix::parse(&p).ok_or(ConfigError::InvalidKeyPrefix(p))?,
            None => KeyPrefix::DEFAULT,
        };

        Ok(Self {
            default_agent,
            profiles,
            key_prefix,
        })
    }

    /// The profile used when none is named.
    pub fn default_agent(&self) -> &AgentProfile {
        &self.default_agent
    }

    pub fn profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles.get(name)
    }

    /// All profiles, by name.
    pub fn profiles(&self) -> impl Iterator<Item = &AgentProfile> {
        self.profiles.values()
    }

    pub fn key_prefix(&self) -> KeyPrefix {
        self.key_prefix
    }
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
    })
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
