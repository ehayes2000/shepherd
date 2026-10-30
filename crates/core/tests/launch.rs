//! What a new session runs: the spawn spec and run files built from its agent profile, through `launch::plan`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::agents::codex;
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

// ---------- codex ----------

/// The hook command for [`EXE`]: shell-quoted, as Codex runs it with `$SHELL -lc`.
const CODEX_HOOK: &str = "'/opt/shepherd/bin/shepherd' hook codex";

/// The value of the one `-c hooks=...` override, parsed the way Codex parses `-c` values
/// (`codex-rs/utils/cli/src/config_override.rs` `parse_toml_value`: the text after `=` as the TOML value of `_x_`).
fn codex_hooks(launch: &Launch) -> toml::Table {
    let [flag, setting, ..] = &launch.spawn.args[..] else {
        panic!("no -c override in {:?}", launch.spawn.args);
    };
    assert_eq!(flag, "-c");
    let value = setting
        .strip_prefix("hooks=")
        .unwrap_or_else(|| panic!("not a hooks override: {setting}"));
    let parsed: toml::Table = toml::from_str(&format!("_x_ = {value}"))
        .unwrap_or_else(|e| panic!("{value} is not a TOML value: {e}"));
    match parsed.get("_x_") {
        Some(toml::Value::Table(t)) => t.clone(),
        other => panic!("expected an inline table, got {other:?}"),
    }
}

#[test]
fn a_codex_session_gets_shepherds_hooks_as_a_config_override_before_the_profiles_own_args() {
    let launch = plan(&profile(AgentKind::Codex, "codex", &["--model", "o3"], &[]));
    assert_eq!(launch.spawn.command, "codex");
    let args = &launch.spawn.args;
    assert_eq!(args.len(), 4, "{args:?}");
    assert_eq!(args[0], "-c");
    assert!(args[1].starts_with("hooks="), "{args:?}");
    assert_eq!(args[2..], ["--model", "o3"]);
    // Nothing is written: the override is the whole configuration, and `~/.codex` is never touched.
    assert!(launch.files.is_empty());
}

#[test]
fn the_codex_override_runs_this_shepherd_on_every_hook_the_translator_reads() {
    let hooks = codex_hooks(&plan(&profile(AgentKind::Codex, "codex", &[], &[])));
    let expected: toml::Value = toml::from_str::<toml::Table>(&format!(
        "x = [{{ hooks = [{{ type = \"command\", command = {CODEX_HOOK:?}, timeout = 3, async = false }}] }}]"
    ))
    .unwrap()["x"]
        .clone();
    let events: Vec<&str> = hooks
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "state")
        .collect();
    let mut want = codex::HOOK_EVENTS.to_vec();
    want.sort_unstable();
    assert_eq!(events, want);
    for event in codex::HOOK_EVENTS {
        assert_eq!(hooks.get(event), Some(&expected), "{event}");
    }
    assert_eq!(
        codex::HOOK_EVENTS,
        [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "Interrupt",
            "PreCompact",
            "PostCompact",
            "SessionEnd",
        ]
    );
}

#[test]
fn the_codex_override_trusts_exactly_shepherds_own_hooks() {
    let launch = plan(&profile(AgentKind::Codex, "codex", &[], &[]));
    let hooks = codex_hooks(&launch);
    let Some(toml::Value::Table(state)) = hooks.get("state") else {
        panic!("no hooks.state in {hooks:?}");
    };
    assert_eq!(state.len(), codex::HOOK_EVENTS.len(), "{state:?}");
    for event in codex::HOOK_EVENTS {
        let key = codex::hook_trust_key(event).unwrap();
        let hash = codex::hook_trust_hash(event, CODEX_HOOK).unwrap();
        let mut entry = toml::Table::new();
        entry.insert("trusted_hash".to_owned(), toml::Value::String(hash));
        assert_eq!(state.get(&key), Some(&toml::Value::Table(entry)), "{event}");
    }
    // Trust comes only from our own state entries, never from bypassing Codex's hook review.
    assert!(
        !launch
            .spawn
            .args
            .iter()
            .any(|a| a.contains("bypass-hook-trust")),
        "{:?}",
        launch.spawn.args
    );
}

/// Pinned against Codex rust-v0.166.0. The key is `hook_key` in `codex-rs/hooks/src/lib.rs`
/// (`{key_source}:{hook_event_key_label}:{group_index}:{handler_index}`), with the session-flags layer's
/// `key_source` from `config_toml_source_path` in `codex-rs/hooks/src/engine/discovery.rs`
/// (`synthetic_layer_path("<session-flags>/config.toml")`, resolved against `/`). The hash is `hook_hash` in that
/// file over the normalized handler (explicit `timeout`, no `commandWindows`, no matcher for these events), through
/// `version_for_toml` in `codex-rs/config/src/fingerprint.rs`: sha256 of the key-sorted compact JSON
/// `{"event_name":"stop","hooks":[{"async":false,"command":"'/opt/shepherd/bin/shepherd' hook codex","timeout":3,"type":"command"}]}`,
/// computed independently with Python's `hashlib` and `json.dumps(sort_keys=True, separators=(",", ":"))`.
#[test]
fn the_codex_trust_key_and_hash_match_codexs_own_format() {
    assert_eq!(
        codex::hook_trust_key("Stop").as_deref(),
        Some("/<session-flags>/config.toml:stop:0:0")
    );
    assert_eq!(
        codex::hook_trust_key("PreToolUse").as_deref(),
        Some("/<session-flags>/config.toml:pre_tool_use:0:0")
    );
    assert_eq!(
        codex::hook_trust_hash("Stop", CODEX_HOOK).as_deref(),
        Some("sha256:bff6ae6bbbb80079d110899cbcb15227c5eab592a5a4ab8038a0025144d5a820")
    );
    assert_eq!(
        codex::hook_trust_hash("PreToolUse", CODEX_HOOK).as_deref(),
        Some("sha256:f6f0e825a6371da868a37d1036b1d90b2ced11259a1a5a3405c57c6c1699fdc0")
    );
    assert_eq!(
        codex::hook_trust_hash("SessionStart", CODEX_HOOK).as_deref(),
        Some("sha256:c58aeb12532c4e64a0dd97d01f1ec3d5543ee3b60d392db3260d65b38261a824")
    );
    assert_eq!(codex::hook_trust_key("NotAHook"), None);
    assert_eq!(codex::hook_trust_hash("NotAHook", CODEX_HOOK), None);
}

#[test]
fn a_shepherd_path_with_spaces_is_quoted_for_the_shell() {
    let mut ctx = ctx();
    ctx.shepherd = PathBuf::from("/Users/me/Library/Application Support/shepherd");
    let launch = launch::plan(&profile(AgentKind::Codex, "codex", &[], &[]), &ctx).unwrap();
    let hooks = codex_hooks(&launch);
    let command = &hooks["Stop"][0]["hooks"][0]["command"];
    assert_eq!(
        command.as_str(),
        Some("'/Users/me/Library/Application Support/shepherd' hook codex")
    );
    let key = codex::hook_trust_key("Stop").unwrap();
    assert_eq!(
        hooks["state"][&key]["trusted_hash"].as_str(),
        Some("sha256:118f56800bd65b9fa3a351f6f202fea1743c6d7899c66a2164156e24afe35c44")
    );
}

#[test]
fn a_shepherd_path_the_shell_cannot_take_quoted_is_refused() {
    for path in ["/opt/it's/shepherd", "/opt/back\\slash/shepherd"] {
        let mut ctx = ctx();
        ctx.shepherd = PathBuf::from(path);
        let result = launch::plan(&profile(AgentKind::Codex, "codex", &[], &[]), &ctx);
        assert_eq!(result, Err(LaunchError::NotShellSafe(PathBuf::from(path))));
    }
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
fn shell_sessions_run_the_profile_as_written_with_no_run_files() {
    let launch = plan(&profile(AgentKind::Shell, "agent", &["-x"], &[]));
    assert_eq!(launch.spawn.command, "agent");
    assert_eq!(launch.spawn.args, ["-x"]);
    assert!(launch.files.is_empty());
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

// ---------- resume ----------

fn plan_resume(profile: &AgentProfile, agent_session: &str) -> Launch {
    launch::plan_resume(profile, &ctx(), agent_session).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn resuming_a_claude_session_adds_the_resume_flag_after_the_settings_and_before_the_profiles_args()
{
    let p = profile(AgentKind::ClaudeCode, "claude", &["--model", "opus"], &[]);
    let launch = plan_resume(&p, "abc123");
    assert_eq!(
        launch.spawn.args,
        [
            "--settings",
            "/home/me/.shepherd/run/s_1/settings.json",
            "--resume",
            "abc123",
            "--model",
            "opus"
        ]
    );
}

#[test]
fn resuming_a_codex_session_runs_the_resume_subcommand_after_the_hooks_override() {
    let p = profile(AgentKind::Codex, "codex", &["--model", "o3"], &[]);
    let launch = plan_resume(&p, "019a-uuid");
    let args = &launch.spawn.args;
    assert_eq!(args.len(), 6, "{args:?}");
    assert_eq!(args[0], "-c");
    assert!(args[1].starts_with("hooks="), "{args:?}");
    assert_eq!(args[2..], ["resume", "019a-uuid", "--model", "o3"]);
}

#[test]
fn a_resumed_session_gets_the_same_hooks_files_env_and_cwd_as_a_new_one() {
    for p in [
        profile(AgentKind::ClaudeCode, "claude", &[], &[("A", "1")]),
        profile(AgentKind::Codex, "codex", &[], &[("A", "1")]),
    ] {
        let new = plan(&p);
        let resumed = plan_resume(&p, "id-1");
        assert_eq!(resumed.files, new.files, "{:?}", p.kind);
        assert_eq!(resumed.spawn.env, new.spawn.env, "{:?}", p.kind);
        assert_eq!(resumed.spawn.cwd, new.spawn.cwd, "{:?}", p.kind);
        assert_eq!(resumed.spawn.command, new.spawn.command, "{:?}", p.kind);
        assert_eq!(resumed.spawn.args[..2], new.spawn.args[..2], "{:?}", p.kind);
    }
}

#[test]
fn a_shell_session_cannot_be_resumed() {
    let p = profile(AgentKind::Shell, "zsh", &[], &[]);
    assert_eq!(
        launch::plan_resume(&p, &ctx(), "x"),
        Err(LaunchError::CannotResume(AgentKind::Shell))
    );
}
