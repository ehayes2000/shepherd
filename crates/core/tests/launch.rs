//! What a new session runs: the spawn spec and run files built from its agent profile, through `launch::plan`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::config::{AgentKind, AgentProfile, Config, RawConfig};
use shepherd_core::host::launch::{
    self, Launch, LaunchContext, LaunchError, SESSION_VAR, SETTINGS_FILE, SOCKET_VAR,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::PtySize;

const EXE: &str = "/opt/shepherd/bin/shepherd";

fn ctx() -> LaunchContext {
    LaunchContext {
        session: SessionId::new("s_1"),
        cwd: PathBuf::from("/home/me"),
        size: PtySize { cols: 80, rows: 24 },
        run_dir: PathBuf::from("/home/me/.shepherd/run/s_1"),
        shepherd: PathBuf::from(EXE),
        socket: PathBuf::from("/home/me/.shepherd/host.sock"),
    }
}

fn profile(kind: AgentKind, command: &str, args: &[&str], env: &[(&str, &str)]) -> AgentProfile {
    AgentProfile {
        name: "p".to_owned(),
        kind,
        command: command.to_owned(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn plan(profile: &AgentProfile) -> Launch {
    launch::plan(profile, &ctx()).unwrap_or_else(|e| panic!("{e}"))
}

fn env_map(launch: &Launch) -> BTreeMap<String, String> {
    launch.spawn.env.iter().cloned().collect()
}

/// The generated Claude Code settings, parsed.
fn settings(launch: &Launch) -> Value {
    let file = launch
        .files
        .iter()
        .find(|f| f.name == SETTINGS_FILE)
        .unwrap_or_else(|| panic!("no {SETTINGS_FILE} in {:?}", launch.files));
    serde_json::from_slice(&file.contents).unwrap_or_else(|e| panic!("settings aren't JSON: {e}"))
}

// ---------- claude-code ----------

#[test]
fn a_claude_session_loads_generated_settings_before_the_profiles_own_args() {
    let launch = plan(&profile(
        AgentKind::ClaudeCode,
        "claude",
        &["--model", "opus"],
        &[],
    ));
    assert_eq!(launch.spawn.command, "claude");
    assert_eq!(
        launch.spawn.args,
        [
            "--settings",
            "/home/me/.shepherd/run/s_1/settings.json",
            "--model",
            "opus"
        ]
    );
    assert_eq!(launch.spawn.cwd, PathBuf::from("/home/me"));
    assert_eq!(launch.spawn.size, PtySize { cols: 80, rows: 24 });
    assert_eq!(launch.files.len(), 1);
}

#[test]
fn the_generated_settings_run_this_shepherd_by_absolute_path_for_every_hook_the_translator_reads() {
    let settings = settings(&plan(&profile(AgentKind::ClaudeCode, "claude", &[], &[])));
    let handler = json!([{
        "hooks": [{ "type": "command", "command": EXE, "args": ["hook", "claude-code"] }]
    }]);
    let hooks = settings["hooks"]
        .as_object()
        .unwrap_or_else(|| panic!("no hooks in {settings}"));
    let events: Vec<&str> = hooks.keys().map(String::as_str).collect();
    assert_eq!(
        events,
        [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PermissionDenied",
            "PostToolUse",
            "PostToolUseFailure",
            "Notification",
            "Stop",
            "StopFailure",
            "PreCompact",
            "PostCompact",
            "SessionEnd",
        ]
    );
    for (event, entry) in hooks {
        assert_eq!(entry, &handler, "{event}");
    }
}

#[test]
fn the_generated_settings_set_nothing_but_hooks() {
    let settings = settings(&plan(&profile(AgentKind::ClaudeCode, "claude", &[], &[])));
    let keys: Vec<&String> = settings.as_object().map(|o| o.keys().collect()).unwrap();
    assert_eq!(keys, ["hooks"]);
}

// ---------- every kind ----------

#[test]
fn every_session_gets_its_id_and_the_host_socket_on_top_of_the_profile_env() {
    for kind in [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Shell] {
        let launch = plan(&profile(kind, "x", &[], &[("FOO", "bar")]));
        let env = env_map(&launch);
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"), "{kind:?}");
        assert_eq!(
            env.get(SESSION_VAR).map(String::as_str),
            Some("s_1"),
            "{kind:?}"
        );
        assert_eq!(
            env.get(SOCKET_VAR).map(String::as_str),
            Some("/home/me/.shepherd/host.sock"),
            "{kind:?}"
        );
    }
}

#[test]
fn shepherds_variables_win_over_a_profile_setting_the_same_name() {
    let launch = plan(&profile(
        AgentKind::Shell,
        "sh",
        &[],
        &[(SESSION_VAR, "spoofed"), (SOCKET_VAR, "/elsewhere")],
    ));
    let names: Vec<&str> = launch.spawn.env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names.iter().filter(|n| **n == SESSION_VAR).count(),
        1,
        "{names:?}"
    );
    let env = env_map(&launch);
    assert_eq!(env[SESSION_VAR], "s_1");
    assert_eq!(env[SOCKET_VAR], "/home/me/.shepherd/host.sock");
}

#[test]
fn shell_and_codex_sessions_run_the_profile_as_written_with_no_run_files() {
    for kind in [AgentKind::Codex, AgentKind::Shell] {
        let launch = plan(&profile(kind, "agent", &["-x"], &[]));
        assert_eq!(launch.spawn.command, "agent");
        assert_eq!(launch.spawn.args, ["-x"], "{kind:?}");
        assert!(launch.files.is_empty(), "{kind:?}");
    }
}

#[test]
fn the_variables_are_the_ones_shepherd_hook_reads() {
    assert_eq!(SESSION_VAR, "SHEPHERD_SESSION");
    assert_eq!(SOCKET_VAR, "SHEPHERD_SOCK");
}

#[test]
fn a_path_that_is_not_utf8_cannot_be_written_into_the_settings() {
    let bad = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/shepherd"));
    let mut ctx = ctx();
    ctx.shepherd = bad.clone();
    let result = launch::plan(&profile(AgentKind::ClaudeCode, "claude", &[], &[]), &ctx);
    assert_eq!(result, Err(LaunchError::NotUtf8(bad)));
}

// ---------- which profile ----------

#[test]
fn a_new_session_without_a_profile_uses_the_default_agent() {
    let raw: RawConfig =
        serde_json::from_value(json!({ "defaults": { "agent": "shell" } })).unwrap();
    let config = Config::resolve(Some(raw), Some("/bin/zsh".to_owned())).unwrap();
    assert_eq!(
        config.profile_or_default(None).map(|p| &p.name[..]),
        Some("shell")
    );
    assert_eq!(
        config
            .profile_or_default(Some("claude"))
            .map(|p| &p.name[..]),
        Some("claude")
    );
    assert_eq!(config.profile_or_default(Some("nope")), None);
}
