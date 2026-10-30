//! Config resolution: built-in defaults, merge rules and validation, through `Config::resolve`.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use shepherd_core::config::{AgentKind, Config, ConfigError, KeyPrefix, RawConfig};

/// A `RawConfig` from JSON shaped like the TOML file.
fn raw(value: Value) -> RawConfig {
    serde_json::from_value(value).unwrap_or_else(|e| panic!("bad RawConfig: {e}"))
}

fn resolve(value: Value) -> Result<Config, ConfigError> {
    Config::resolve(Some(raw(value)), None)
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------- built-in defaults ----------

#[test]
fn missing_file_gives_builtin_profiles() {
    let config = Config::resolve(None, None).unwrap();

    let names: Vec<_> = config.profiles().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["claude", "codex", "shell"]);

    let claude = config.profile("claude").unwrap();
    assert_eq!(claude.kind, AgentKind::ClaudeCode);
    assert_eq!(claude.command, "claude");
    assert!(claude.args.is_empty());
    assert!(claude.env.is_empty());

    let codex = config.profile("codex").unwrap();
    assert_eq!(
        (codex.kind, codex.command.as_str()),
        (AgentKind::Codex, "codex")
    );

    let shell = config.profile("shell").unwrap();
    assert_eq!(
        (shell.kind, shell.command.as_str()),
        (AgentKind::Shell, "sh")
    );
}

#[test]
fn missing_file_defaults_to_claude_and_ctrl_b() {
    let config = Config::resolve(None, None).unwrap();
    assert_eq!(config.default_agent().name, "claude");
    assert_eq!(config.key_prefix(), KeyPrefix::DEFAULT);
    assert_eq!(config.key_prefix().to_string(), "ctrl-b");
}

#[test]
fn empty_file_is_the_same_as_missing_file() {
    assert_eq!(
        resolve(json!({})).unwrap(),
        Config::resolve(None, None).unwrap()
    );
}

// ---------- merge rules ----------

#[test]
fn defaults_agent_picks_the_default_profile() {
    let config = resolve(json!({ "defaults": { "agent": "codex" } })).unwrap();
    assert_eq!(config.default_agent().name, "codex");
}

#[test]
fn user_table_overlays_builtin_field_by_field() {
    let config = resolve(json!({ "agents": { "claude": { "args": ["--verbose"] } } })).unwrap();
    let claude = config.profile("claude").unwrap();
    assert_eq!(
        claude.kind,
        AgentKind::ClaudeCode,
        "kind kept from built-in"
    );
    assert_eq!(claude.command, "claude", "command kept from built-in");
    assert_eq!(claude.args, ["--verbose"]);
}

#[test]
fn user_command_replaces_builtin_command() {
    let config = resolve(json!({ "agents": { "shell": { "command": "/bin/zsh" } } })).unwrap();
    let shell = config.profile("shell").unwrap();
    assert_eq!(shell.command, "/bin/zsh");
    assert_eq!(shell.kind, AgentKind::Shell);
}

#[test]
fn args_replace_wholesale() {
    let first = resolve(json!({ "agents": { "x": {
        "kind": "shell", "command": "sh", "args": ["-l", "-i"] } } }))
    .unwrap();
    assert_eq!(first.profile("x").unwrap().args, ["-l", "-i"]);

    // Overriding a built-in with an empty list clears its args rather than appending.
    let cleared = resolve(json!({ "agents": { "claude": { "args": [] } } })).unwrap();
    assert!(cleared.profile("claude").unwrap().args.is_empty());
}

#[test]
fn defaults_env_applies_to_every_profile() {
    let config = resolve(json!({ "defaults": { "env": { "TERM_PROGRAM": "shepherd" } } })).unwrap();
    for profile in config.profiles() {
        assert_eq!(
            profile.env,
            env(&[("TERM_PROGRAM", "shepherd")]),
            "{}",
            profile.name
        );
    }
}

#[test]
fn profile_env_merges_over_defaults_env_by_key() {
    let config = resolve(json!({
        "defaults": { "env": { "A": "default", "B": "default" } },
        "agents": { "codex": { "env": { "B": "codex", "C": "codex" } } },
    }))
    .unwrap();
    assert_eq!(
        config.profile("codex").unwrap().env,
        env(&[("A", "default"), ("B", "codex"), ("C", "codex")])
    );
    assert_eq!(
        config.profile("claude").unwrap().env,
        env(&[("A", "default"), ("B", "default")])
    );
}

#[test]
fn new_profile_with_kind_and_command_is_added_beside_builtins() {
    let config = resolve(json!({
        "defaults": { "agent": "yolo" },
        "agents": { "yolo": {
            "kind": "claude-code",
            "command": "claude",
            "args": ["--dangerously-skip-permissions"],
            "env": { "X": "1" },
        } },
    }))
    .unwrap();

    let names: Vec<_> = config.profiles().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["claude", "codex", "shell", "yolo"]);

    let yolo = config.default_agent();
    assert_eq!(yolo.name, "yolo");
    assert_eq!(yolo.kind, AgentKind::ClaudeCode);
    assert_eq!(yolo.command, "claude");
    assert_eq!(yolo.args, ["--dangerously-skip-permissions"]);
    assert_eq!(yolo.env, env(&[("X", "1")]));
}

#[test]
fn key_prefix_accepts_ctrl_letter_and_ctrl_space() {
    let a = resolve(json!({ "keys": { "prefix": "ctrl-a" } })).unwrap();
    assert_eq!(a.key_prefix().ctrl_key(), 'a');
    assert_eq!(a.key_prefix().to_string(), "ctrl-a");

    let space = resolve(json!({ "keys": { "prefix": "ctrl-space" } })).unwrap();
    assert_eq!(space.key_prefix().ctrl_key(), ' ');
    assert_eq!(space.key_prefix().to_string(), "ctrl-space");
}

#[test]
fn the_attention_bell_is_off_unless_turned_on() {
    assert!(!Config::resolve(None, None).unwrap().notify_bell());
    assert!(!resolve(json!({ "notify": {} })).unwrap().notify_bell());
    assert!(
        resolve(json!({ "notify": { "bell": true } }))
            .unwrap()
            .notify_bell()
    );
}

#[test]
fn an_unknown_notify_field_is_rejected() {
    let result = serde_json::from_value::<RawConfig>(json!({ "notify": { "bel": true } }));
    assert!(result.is_err());
}

// ---------- validation ----------

#[test]
fn unknown_kind_is_an_error() {
    assert_eq!(
        resolve(json!({ "agents": { "x": { "kind": "gemini", "command": "gemini" } } })),
        Err(ConfigError::UnknownKind {
            agent: "x".into(),
            kind: "gemini".into()
        })
    );
    // Also when overriding a built-in.
    assert_eq!(
        resolve(json!({ "agents": { "claude": { "kind": "Claude-Code" } } })),
        Err(ConfigError::UnknownKind {
            agent: "claude".into(),
            kind: "Claude-Code".into()
        })
    );
}

#[test]
fn new_profile_without_kind_is_an_error() {
    assert_eq!(
        resolve(json!({ "agents": { "x": { "command": "x" } } })),
        Err(ConfigError::MissingField {
            agent: "x".into(),
            field: "kind"
        })
    );
}

#[test]
fn new_profile_without_command_is_an_error() {
    assert_eq!(
        resolve(json!({ "agents": { "x": { "kind": "shell" } } })),
        Err(ConfigError::MissingField {
            agent: "x".into(),
            field: "command"
        })
    );
}

#[test]
fn empty_command_is_an_error() {
    assert_eq!(
        resolve(json!({ "agents": { "claude": { "command": "  " } } })),
        Err(ConfigError::EmptyCommand {
            agent: "claude".into()
        })
    );
}

#[test]
fn defaults_agent_naming_unknown_profile_is_an_error() {
    assert_eq!(
        resolve(json!({ "defaults": { "agent": "gemini" } })),
        Err(ConfigError::UnknownDefaultAgent("gemini".into()))
    );
}

#[test]
fn malformed_key_prefix_is_an_error() {
    for bad in [
        "ctrl-B", "ctrl-", "ctrl-ab", "alt-b", "b", "ctrl-1", "ctrl- ",
    ] {
        assert_eq!(
            resolve(json!({ "keys": { "prefix": bad } })),
            Err(ConfigError::InvalidKeyPrefix(bad.into())),
            "{bad:?}"
        );
    }
}

#[test]
fn raw_config_rejects_unknown_fields() {
    let typo: Result<RawConfig, _> = serde_json::from_value(json!({ "default": {} }));
    assert!(typo.is_err());
    let agent_typo: Result<RawConfig, _> =
        serde_json::from_value(json!({ "agents": { "claude": { "comand": "x" } } }));
    assert!(agent_typo.is_err());
}

// ---------- login shell ----------

#[test]
fn builtin_shell_uses_the_login_shell() {
    let config = Config::resolve(None, Some("/bin/zsh".into())).unwrap();
    let shell = config.profile("shell").unwrap();
    assert_eq!(
        (shell.kind, shell.command.as_str()),
        (AgentKind::Shell, "/bin/zsh")
    );
}

#[test]
fn builtin_shell_without_a_login_shell_is_sh() {
    let config = Config::resolve(None, None).unwrap();
    assert_eq!(config.profile("shell").unwrap().command, "sh");
}

#[test]
fn user_set_sh_wins_over_the_login_shell() {
    let raw = raw(json!({ "agents": { "shell": { "command": "sh" } } }));
    let config = Config::resolve(Some(raw), Some("/bin/zsh".into())).unwrap();
    assert_eq!(config.profile("shell").unwrap().command, "sh");
}

#[test]
fn login_shell_only_affects_the_shell_profile() {
    let config = Config::resolve(None, Some("/bin/zsh".into())).unwrap();
    assert_eq!(config.profile("claude").unwrap().command, "claude");
    assert_eq!(config.profile("codex").unwrap().command, "codex");
}
