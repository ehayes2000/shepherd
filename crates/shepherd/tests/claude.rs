//! Claude Code sessions: the host launches `claude` from its profile with shepherd's hooks in a generated settings
//! file, and the session's status follows the hooks. On the fake ports with a fake `claude`; the run files are real
//! files in the harness's tempdir. Never the real `claude`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd::hook::{self, HookError};
use shepherd_core::acpp::Seq;
use shepherd_core::config::{AgentKind, RawAgent, RawConfig};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LogId, PtySize, SpawnSpec};
use shepherd_core::session::{Activity, SessionState};
use shepherd_core::wire::{
    BuildId, ClientRequest, DigestHasher, HookFrame, HostUpgrade, InputKind, SessionClose,
    SessionNew, SessionResume, UpgradeAnswer, WireError,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::FakeNetwork;
use shepherd_testkit::fakes::pty::ProgramInput;
use support::{
    FakeClient, HOME, HarnessBuilder, HostHarness, Program, SHEPHERD_EXE, SOCKET, entries,
    new_session_id, response,
};

/// The command of the built-in `claude` profile.
const CLAUDE: &str = "claude";
/// A liveness bound for hook threads, never reached when the host is stepped.
const GENEROUS: Duration = Duration::from_secs(60);

type Hooks = Arc<Mutex<Vec<JoinHandle<Result<(), HookError>>>>>;
type Spawned = Arc<Mutex<Vec<SpawnSpec>>>;
type Typed = Arc<Mutex<Vec<u8>>>;

/// The value after `--settings` in `args`.
fn settings_arg(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--settings")?;
    args.get(at + 1).map(PathBuf::from)
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A fake `claude`. It records how it was spawned. Typed a hook payload (one JSON line), it does what Claude Code
/// does with the settings it was given: looks up the handler for the payload's `hook_event_name` in the
/// `--settings` file and runs it with the payload on stdin and its own environment. Running is simulated: a handler
/// whose command is [`SHEPHERD_EXE`] is `shepherd`, so the fake calls `shepherd hook <args[1]>` in-process, on its
/// own thread like a separate hook process. Any other handler, or none, does nothing. Typed `print:<text>`, it prints
/// `<text>`, as its TUI redrawing. Resized, it asks the terminal for its attributes (DA1) and redraws, and so does a
/// focus report (`CSI I`). Everything typed into it is kept in `typed`.
fn fake_claude(net: &FakeNetwork, hooks: &Hooks, spawned: &Spawned, typed: &Typed) -> Program {
    let (net, hooks, spawned, typed) = (net.clone(), hooks.clone(), spawned.clone(), typed.clone());
    Arc::new(move |input, p| {
        let spec = p.spec().clone();
        if let ProgramInput::Stdin(bytes) = input {
            typed.lock().unwrap().extend_from_slice(bytes);
        }
        let bytes = match input {
            ProgramInput::Start => return spawned.lock().unwrap().push(spec),
            ProgramInput::Stdin(b"/exit\r") => return p.exit(0),
            ProgramInput::Stdin(b"\x1b[I") => return p.print(b"redrawn"),
            ProgramInput::Stdin(bytes) => bytes,
            ProgramInput::Resized => return p.print(b"\x1b[credrawn"),
        };
        if let Some(text) = bytes.strip_prefix(b"print:") {
            return p.print(text);
        }
        let Ok(payload) = serde_json::from_slice::<Value>(bytes.trim_ascii_end()) else {
            return;
        };
        let Some(settings) = settings_arg(&spec.args) else {
            return;
        };
        let settings = read_json(&settings);
        let event = payload["hook_event_name"].as_str().unwrap_or_default();
        let handler = &settings["hooks"][event][0]["hooks"][0];
        let args: Vec<&str> = handler["args"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let (Some(SHEPHERD_EXE), ["hook", agent]) = (handler["command"].as_str(), &args[..]) else {
            return;
        };
        let session = spec
            .env
            .iter()
            .find(|(k, _)| k == hook::SESSION_VAR)
            .map(|(_, v)| v.as_str());
        let stdin = serde_json::to_vec(&payload).unwrap();
        let Some(frame) = hook::hook_frame(agent, session, &stdin) else {
            return;
        };
        let net = net.clone();
        let handle = std::thread::spawn(move || {
            hook::deliver(
                Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string())),
                frame,
                GENEROUS,
            )
        });
        hooks.lock().unwrap().push(handle);
    })
}

/// A harness whose `claude` command runs [`fake_claude`].
struct Claude {
    h: HostHarness,
    hooks: Hooks,
    spawned: Spawned,
    typed: Typed,
}

impl Claude {
    fn start() -> Self {
        Self::with_config(support::shell_default_config())
    }

    fn with_config(config: RawConfig) -> Self {
        Self::with(|b| b.config(config))
    }

    /// Starts on what `setup` makes of a builder with [`support::shell_default_config`].
    fn with(setup: impl FnOnce(HarnessBuilder) -> HarnessBuilder) -> Self {
        let net = FakeNetwork::new();
        let hooks = Hooks::default();
        let spawned = Spawned::default();
        let typed = Typed::default();
        let builder = HostHarness::builder()
            .network(net.clone())
            .config(support::shell_default_config())
            .program(CLAUDE, fake_claude(&net, &hooks, &spawned, &typed));
        let h = setup(builder).start();
        Self {
            h,
            hooks,
            spawned,
            typed,
        }
    }

    /// Creates a session with `profile` and returns its id, or the host's error.
    fn new_session(
        &mut self,
        client: &mut FakeClient,
        profile: Option<&str>,
    ) -> Result<SessionId, WireError> {
        self.new_session_in(client, profile, None)
    }

    /// Creates a session with `profile` starting in `cwd`, and returns its id, or the host's error.
    fn new_session_in(
        &mut self,
        client: &mut FakeClient,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<SessionId, WireError> {
        let sent = client.request(ClientRequest::SessionNew(SessionNew {
            profile: profile.map(str::to_owned),
            cwd: cwd.map(str::to_owned),
            branch: None,
            size: None,
        }));
        self.h.step();
        response(&client.take(), &sent)
            .map(|r| SessionId::new(r["session"].as_str().expect("session id")))
    }

    /// The one spawn of `claude` so far.
    fn spawned(&self) -> SpawnSpec {
        let spawned = self.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1, "{spawned:?}");
        spawned[0].clone()
    }

    /// Steps the host until every hook the fake `claude` ran has its answer.
    fn run_hooks(&mut self) -> Vec<Result<(), HookError>> {
        let handles: Vec<_> = self.hooks.lock().unwrap().drain(..).collect();
        let mut results = Vec::new();
        for handle in handles {
            while !handle.is_finished() {
                self.h.step();
                std::thread::yield_now();
            }
            results.push(handle.join().unwrap());
        }
        self.h.step();
        results
    }
}

fn env_var<'a>(spec: &'a SpawnSpec, name: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_claude_session_runs_claude_with_generated_hook_settings_and_shepherds_variables() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();

    let spec = c.spawned();
    assert_eq!(spec.command, CLAUDE);
    assert_eq!(spec.cwd, PathBuf::from(HOME));
    assert_eq!(spec.size, PtySize { cols: 80, rows: 24 });
    assert_eq!(env_var(&spec, "SHEPHERD_SESSION"), Some(session.as_str()));
    assert_eq!(env_var(&spec, "SHEPHERD_SOCK"), Some(SOCKET));

    let dir = c.h.run_root().join(session.as_str());
    let settings = dir.join("settings.json");
    assert_eq!(spec.args, ["--settings", settings.to_str().unwrap()]);
    assert_eq!(mode(c.h.run_root()), 0o700);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&settings), 0o600);
    assert_eq!(
        read_json(&settings)["hooks"]["Stop"],
        json!([{ "hooks": [{ "type": "command", "command": SHEPHERD_EXE, "args": ["hook", "claude-code"] }] }])
    );
}

#[test]
fn closing_a_claude_session_removes_its_run_files() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    let dir = c.h.run_root().join(session.as_str());
    assert!(dir.join("settings.json").exists());

    let sent = client.request(ClientRequest::SessionClose(SessionClose {
        session: session.clone(),
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
    }));
    c.h.step();
    assert_eq!(response(&client.take(), &sent), Ok(json!({})));
    assert!(!dir.exists(), "{} is still there", dir.display());
    // Only that session's directory: the run root stays.
    assert!(c.h.run_root().exists());
}

#[test]
fn a_shell_session_has_no_run_files() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("shell")).unwrap();
    assert!(!c.h.run_root().join(session.as_str()).exists());
}

#[test]
fn a_new_session_without_a_profile_runs_the_default_agent() {
    // No `[defaults] agent`: the built-in default, `claude`.
    let mut c = Claude::with_config(RawConfig::default());
    let mut client = c.h.connect();
    let id = client.new_session();
    c.h.step();
    new_session_id(&client.take(), &id);
    assert_eq!(c.spawned().command, CLAUDE);
}

#[test]
fn a_new_session_with_an_unknown_profile_is_refused_and_creates_nothing() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let result = c.new_session(&mut client, Some("nope"));
    assert!(
        matches!(&result, Err(WireError::InvalidParams(m)) if m.contains("nope")),
        "{result:?}"
    );
    assert!(c.h.host().sessions().is_empty());
    assert!(c.h.log().logs().unwrap().is_empty());
}

#[test]
fn a_claude_session_whose_run_files_cannot_be_written_shows_as_exited_without_spawning() {
    let mut c = Claude::start();
    // A run root others can read is refused, never chmodded (decision 0010).
    std::fs::create_dir(c.h.run_root()).unwrap();
    std::fs::set_permissions(c.h.run_root(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    assert!(c.spawned.lock().unwrap().is_empty());
    let listed = c.h.host().sessions();
    assert!(
        listed.iter().any(|s| s.id == session && s.exited),
        "{listed:?}"
    );
}

#[test]
fn a_claude_session_gets_a_claude_placeholder_name_and_records_its_agent_kind() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();

    let listed = c.h.host().sessions();
    assert_eq!(listed[0].name, "claude-1", "{listed:?}");
    let log = c.h.log().read(LogId::Session(session), Seq::ZERO).unwrap();
    let created = serde_json::to_value(log[0].frame()).unwrap();
    assert_eq!(created["method"], "_shepherd/session/created");
    assert_eq!(created["params"]["agent"], "claude-code");
}

#[test]
fn a_new_session_starts_in_the_directory_it_asks_for() {
    let mut c = Claude::start();
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    c.new_session_in(&mut client, Some("claude"), dir.path().to_str())
        .unwrap();
    // Resolved: on macOS the tempdir is behind the `/var` symlink.
    assert_eq!(c.spawned().cwd, std::fs::canonicalize(dir.path()).unwrap());
}

/// Asks for a session starting in `cwd`, and checks it was refused with `InvalidParams` and nothing was created.
fn assert_cwd_refused(cwd: &str) {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let result = c.new_session_in(&mut client, Some("claude"), Some(cwd));
    assert!(
        matches!(&result, Err(WireError::InvalidParams(m)) if m.contains(cwd)),
        "{result:?}"
    );
    assert!(c.h.host().sessions().is_empty());
    assert!(c.h.log().logs().unwrap().is_empty());
    assert!(c.spawned.lock().unwrap().is_empty());
    assert!(!c.h.run_root().exists());
}

#[test]
fn a_new_session_in_a_missing_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert_cwd_refused(dir.path().join("missing").to_str().unwrap());
}

#[test]
fn a_new_session_in_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    assert_cwd_refused(file.to_str().unwrap());
}

#[test]
fn a_new_session_in_a_relative_directory_is_refused() {
    // Relative to wherever the host happened to start, which the client can't know.
    assert_cwd_refused(".");
}

#[test]
fn a_claude_session_whose_pty_exits_has_its_run_files_removed() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    let dir = c.h.run_root().join(session.as_str());
    assert!(dir.join("settings.json").exists());

    client.input(&session, b"/exit\r");
    c.h.step();
    let listed = c.h.host().sessions();
    assert!(
        listed.iter().any(|s| s.id == session && s.exited),
        "{listed:?}"
    );
    assert!(!dir.exists(), "{} is still there", dir.display());
}

#[test]
fn a_session_end_hook_keeps_the_run_files_until_the_pty_exits() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    let dir = c.h.run_root().join(session.as_str());

    // Claude's own `agent/exited` (source `hook`): its process, and the settings it reads, may live on.
    let mut line = serde_json::to_vec(&fixture("SessionEnd.other")).unwrap();
    line.push(b'\r');
    client.input(&session, &line);
    c.h.step();
    assert_eq!(c.run_hooks(), vec![Ok(())]);
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    assert!(
        log.iter()
            .any(|e| e.frame().method() == Some("_shepherd/agent/exited")),
        "the hook recorded no exit: {log:?}"
    );
    assert!(dir.join("settings.json").exists());

    client.input(&session, b"/exit\r");
    c.h.step();
    assert!(!dir.exists(), "{} is still there", dir.display());
}

#[test]
fn stale_run_directories_are_swept_at_startup_without_following_symlinks() {
    let mut c = Claude::start();
    let run_root = c.h.run_root().to_path_buf();
    let outside = tempfile::tempdir().unwrap();
    let precious = outside.path().join("precious");
    std::fs::create_dir(&precious).unwrap();
    std::fs::write(precious.join("keep"), b"mine").unwrap();
    std::fs::create_dir_all(run_root.join("s_old")).unwrap();
    std::fs::write(run_root.join("s_old").join("settings.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(&precious, run_root.join("s_old").join("inner")).unwrap();
    std::os::unix::fs::symlink(&precious, run_root.join("s_link")).unwrap();

    c.h.restart();

    assert!(!run_root.join("s_old").exists());
    // The link isn't a directory shepherd made: neither it nor what it points to is touched.
    assert!(
        std::fs::symlink_metadata(run_root.join("s_link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(precious.join("keep")).unwrap(), b"mine");
    assert!(run_root.exists());
}

#[test]
fn a_symlinked_run_root_is_not_swept() {
    let mut c = Claude::start();
    let run_root = c.h.run_root().to_path_buf();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("s_old")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &run_root).unwrap();

    c.h.restart();

    assert!(outside.path().join("s_old").exists());
}

#[test]
fn settings_written_over_a_longer_stale_file_replace_it_entirely() {
    let mut c = Claude::start();
    // The id the host gives its first session.
    let stale = c.h.run_root().join("s_t1");
    // Private, as the host made them (decision 0010): only the length is stale.
    for dir in [c.h.run_root(), stale.as_path()] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(stale.join("settings.json"), vec![b' '; 64 * 1024]).unwrap();
    std::fs::set_permissions(
        stale.join("settings.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    assert_eq!(session.as_str(), "s_t1");
    let written = std::fs::read(stale.join("settings.json")).unwrap();
    assert!(written.len() < 64 * 1024, "{} bytes", written.len());
    let settings: Value = serde_json::from_slice(&written).unwrap();
    assert!(settings["hooks"]["Stop"].is_array(), "{settings}");
}

#[test]
fn a_session_core_refuses_leaves_no_run_files_behind() {
    // A previous host already used the id this host gives its first session.
    let mut prev = HostDriver::new();
    prev.new_session("s_t1");
    let log = FakeEventLog::new()
        .with_envelopes(LogId::Session(SessionId::new("s_t1")), prev.log().to_vec());
    let mut c = Claude::with(|b| b.log(log));
    let mut client = c.h.connect();

    let result = c.new_session(&mut client, Some("claude"));
    assert!(matches!(result, Err(WireError::Internal(_))), "{result:?}");
    assert!(c.spawned.lock().unwrap().is_empty());
    assert!(!c.h.run_root().join("s_t1").exists());
    assert_eq!(c.h.runtime().pending_run_files(), 0);
}

/// What the sidebar shows for an activity, without the details.
fn status(activity: &Activity) -> &'static str {
    match activity {
        Activity::Working { .. } => "Working",
        Activity::NeedsPermission { .. } => "NeedsPermission",
        Activity::Idle => "Idle",
        Activity::Starting => "Starting",
        other => panic!("unexpected {other:?}"),
    }
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures/claude")
        .join(format!("{name}.json"));
    read_json(&path)
}

#[test]
fn a_claude_sessions_status_follows_its_hooks_through_a_permission_prompt() {
    let mut c = Claude::start();
    let mut watcher = c.h.connect();
    watcher.subscribe_all();
    c.h.step();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();

    // Folded from what a subscribed client receives, as the sidebar is.
    let mut state = SessionState::default();
    let mut seen = vec![];
    for hook in [
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
        "PostToolUse.Bash",
        "Stop",
    ] {
        let mut line = serde_json::to_vec(&fixture(hook)).unwrap();
        line.push(b'\r');
        client.input(&session, &line);
        c.h.step();
        assert_eq!(c.run_hooks(), vec![Ok(())], "{hook}");
        for env in entries(&watcher.take()) {
            if env.session() == Some(&session) {
                state.apply(&env);
            }
        }
        let now = status(state.activity());
        if seen.last() != Some(&now) {
            seen.push(now);
        }
    }
    assert_eq!(seen, ["Working", "NeedsPermission", "Working", "Idle"]);
}

// ---------- quiet idle ----------

#[test]
fn a_claude_turn_that_ends_without_a_stop_hook_goes_idle_once_its_pty_is_quiet() {
    // Esc interrupts a turn without a `Stop` hook: nothing says it ended but the spinner stopping.
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    c.hook(&mut client, &session, "UserPromptSubmit");
    assert_eq!(status(c.activity(&session).activity()), "Working");
    let quiet = Duration::from_secs(3);
    assert_eq!(
        c.h.runtime().next_deadline(),
        Some(c.h.clock().now() + quiet)
    );

    // The spinner prints, then stops.
    c.h.clock().advance(Duration::from_secs(2));
    client.input(&session, b"print:spinner");
    c.h.step();
    assert_eq!(
        c.h.runtime().next_deadline(),
        Some(c.h.clock().now() + quiet)
    );
    c.h.clock().advance(Duration::from_millis(2900));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "Working");
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 0);

    c.h.clock().advance(Duration::from_millis(100));
    c.h.step();
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    let last = log.last().unwrap();
    assert_eq!(last.frame().method(), Some("_shepherd/agent/idle"));
    assert_eq!(
        serde_json::to_value(last.frame()).unwrap()["params"],
        json!({ "inferred": "quiet" })
    );
    assert_eq!(status(c.activity(&session).activity()), "Idle");
    assert_eq!(c.h.runtime().next_deadline(), None);

    // Redraws while it waits don't make it working again.
    client.input(&session, b"print:prompt");
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 1);
}

#[test]
fn a_claude_permission_dialog_answered_without_a_hook_goes_idle_once_its_pty_is_quiet() {
    // Choosing "No" fires no hook: the user types, the dialog redraws, and the agent waits at its prompt.
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    for hook in [
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
    ] {
        c.hook(&mut client, &session, hook);
    }
    assert_eq!(status(c.activity(&session).activity()), "NeedsPermission");

    // Redraws nobody asked for (a resize) and quiet don't clear it.
    client.resize(&session, 100, 30);
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "NeedsPermission");
    assert_eq!(c.h.runtime().next_deadline(), None);

    // The user answers and the dialog redraws, then it goes quiet.
    client.input(&session, b"print:no");
    c.h.step();
    c.h.clock().advance(Duration::from_millis(2999));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "NeedsPermission");
    c.h.clock().advance(Duration::from_millis(1));
    c.h.step();
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    assert_eq!(
        serde_json::to_value(log.last().unwrap().frame()).unwrap()["params"],
        json!({ "inferred": "quiet-after-permission" })
    );
    assert_eq!(status(c.activity(&session).activity()), "Idle");
}

#[test]
fn a_permission_dialog_stays_put_through_screen_query_replies_and_focus_reports() {
    // Neither is the user answering, though both are written to the PTY after the request and the dialog redraws.
    // (A focus report starts with ESC, so this doesn't show the client's kind reaching the quiet watch: see
    // `a_paste_answers_a_permission_dialog`.)
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    for hook in [
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
    ] {
        c.hook(&mut client, &session, hook);
    }

    // Resized, it asks for DA1 and the host's screen answers.
    c.typed.lock().unwrap().clear();
    client.resize(&session, 100, 30);
    c.h.step();
    let typed = c.typed.lock().unwrap().clone();
    assert!(
        typed.starts_with(b"\x1b[?") && typed.ends_with(b"c"),
        "the query was answered: {:?}",
        String::from_utf8_lossy(&typed)
    );
    // The window regains focus: the app asked for focus reports, and redraws on one.
    client.input_of(&session, InputKind::Focus, b"\x1b[I");
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "NeedsPermission");
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 0);
}

#[test]
fn a_paste_answers_a_permission_dialog() {
    // A bracketed paste starts with ESC like a cursor key: only its kind, as the client sent it, makes it an answer.
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    for hook in [
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
    ] {
        c.hook(&mut client, &session, hook);
    }

    client.input_of(&session, InputKind::Paste, b"\x1b[200~1\x1b[201~");
    c.h.step();
    // A redraw that isn't typed: the resize redraws the dialog.
    client.resize(&session, 100, 30);
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "Idle");
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 1);
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    assert_eq!(
        serde_json::to_value(log.last().unwrap().frame()).unwrap()["params"],
        json!({ "inferred": "quiet-after-permission" })
    );
}

#[test]
fn an_esc_the_client_marks_as_an_answer_answers_a_permission_dialog() {
    // Esc dismisses the dialog without a hook. Its bytes are an escape sequence like a cursor key: only its kind, as
    // the client sent it, makes it an answer.
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    for hook in [
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
    ] {
        c.hook(&mut client, &session, hook);
    }

    client.input_of(&session, InputKind::Answer, b"\x1b");
    c.h.step();
    // A redraw that isn't typed: the resize redraws the dialog.
    client.resize(&session, 100, 30);
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(status(c.activity(&session).activity()), "Idle");
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 1);
}

#[test]
fn a_shell_session_is_never_inferred_idle() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("shell")).unwrap();
    client.input(&session, b"hello\r");
    c.h.step();
    c.h.clock().advance(Duration::from_secs(60));
    c.h.step();
    assert_eq!(count(&c, &session, "_shepherd/agent/idle"), 0);
    assert_eq!(c.h.runtime().next_deadline(), None);
}

impl Claude {
    /// `session`'s state, folded from its log.
    fn activity(&self, session: &SessionId) -> SessionState {
        let mut state = SessionState::default();
        for env in self
            .h
            .log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap()
        {
            state.apply(&env);
        }
        state
    }
}

// ---------- resume ----------

impl Claude {
    /// Asks to resume `session`, and returns the host's answer.
    fn resume(&mut self, client: &mut FakeClient, session: &SessionId) -> Result<Value, WireError> {
        let sent = client.request(ClientRequest::SessionResume(SessionResume {
            session: session.clone(),
        }));
        self.h.step();
        response(&client.take(), &sent)
    }

    /// Types hook fixture `name` into `session`'s fake `claude` and waits for its hook to be answered.
    fn hook(&mut self, client: &mut FakeClient, session: &SessionId, name: &str) {
        let mut line = serde_json::to_vec(&fixture(name)).unwrap();
        line.push(b'\r');
        client.input(session, &line);
        self.h.step();
        assert_eq!(self.run_hooks(), vec![Ok(())], "{name}");
    }

    fn all_spawned(&self) -> Vec<SpawnSpec> {
        self.spawned.lock().unwrap().clone()
    }
}

#[test]
fn an_exited_claude_session_resumes_its_agent_session_in_the_same_session_after_a_restart() {
    let mut c = Claude::start();
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("claude"), dir.path().to_str())
        .unwrap();
    // `SessionStart.startup` reports agent session `abc123`.
    c.hook(&mut client, &session, "SessionStart.startup");
    c.h.restart();
    assert!(c.h.host().sessions()[0].exited);

    let mut client = c.h.connect();
    assert_eq!(c.resume(&mut client, &session), Ok(json!({})));

    let spawned = c.all_spawned();
    assert_eq!(spawned.len(), 2, "{spawned:?}");
    let resumed = &spawned[1];
    let settings = c.h.run_root().join(session.as_str()).join("settings.json");
    assert_eq!(
        resumed.args,
        [
            "--settings",
            settings.to_str().unwrap(),
            "--resume",
            "abc123"
        ]
    );
    assert_eq!(resumed.cwd, std::fs::canonicalize(dir.path()).unwrap());
    assert_eq!(env_var(resumed, "SHEPHERD_SESSION"), Some(session.as_str()));
    // The hooks are injected again, exactly as for a new session.
    assert_eq!(
        read_json(&settings)["hooks"]["Stop"],
        json!([{ "hooks": [{ "type": "command", "command": SHEPHERD_EXE, "args": ["hook", "claude-code"] }] }])
    );
    // Still the one session, running again, and its log goes on: the resumed agent's hooks land in it.
    let listed = c.h.host().sessions();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(!listed[0].exited);
    let mut client = c.h.connect();
    c.hook(&mut client, &session, "UserPromptSubmit");
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    let methods: Vec<&str> = log.iter().filter_map(|e| e.frame().method()).collect();
    assert_eq!(
        methods[..4],
        [
            "_shepherd/session/created",
            "_shepherd/agent/started",
            "_shepherd/agent/exited",
            "_shepherd/agent/started",
        ],
        "{methods:?}"
    );
    assert!(methods[4..].contains(&"session/prompt"), "{methods:?}");
}

#[test]
fn a_resumed_claude_session_spawns_at_the_size_its_client_last_gave_it() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    client.resize(&session, 132, 40);
    client.input(&session, b"/exit\r");
    c.h.step();
    assert!(c.h.host().sessions()[0].exited);

    assert_eq!(c.resume(&mut client, &session), Ok(json!({})));

    let spawned = c.all_spawned();
    assert_eq!(spawned.len(), 2, "{spawned:?}");
    assert_eq!(
        spawned[1].size,
        PtySize {
            cols: 132,
            rows: 40
        }
    );
}

#[test]
fn a_session_resized_while_exited_after_a_restart_resumes_at_that_size() {
    let mut c = Claude::start();
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("claude"), dir.path().to_str())
        .unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    c.h.restart();

    // The client selects it after reconnecting, and resizes it before it resumes it.
    let mut client = c.h.connect();
    client.resize(&session, 101, 31);
    c.h.step();
    assert_eq!(c.resume(&mut client, &session), Ok(json!({})));

    let spawned = c.all_spawned();
    assert_eq!(spawned.len(), 2, "{spawned:?}");
    assert_eq!(
        spawned[1].size,
        PtySize {
            cols: 101,
            rows: 31
        }
    );
}

#[test]
fn an_adopted_session_resumes_at_the_size_it_was_carried_with() {
    // A host started as one build, with another installed since: it can update in place.
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let mut hasher = DigestHasher::new();
    hasher.update(b"the build the host started as");
    let running = BuildId {
        version: "0.1.0".into(),
        digest: hasher.finish(),
    };
    let mut c = Claude::with(|b| b.build(running, &exe));
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    client.resize(&session, 150, 50);
    c.h.step();
    let asked = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    c.h.step();
    let answer: UpgradeAnswer =
        serde_json::from_value(response(&client.take(), &asked).unwrap()).unwrap();
    assert_eq!(answer, UpgradeAnswer::Accepted);
    c.h.swap(|_| {});

    // The new image never saw a resize: only the carried size says 150x50.
    let mut client = c.h.connect();
    client.input(&session, b"/exit\r");
    c.h.step();
    assert!(c.h.host().sessions()[0].exited);
    assert_eq!(c.resume(&mut client, &session), Ok(json!({})));

    let spawned = c.all_spawned();
    assert_eq!(spawned.len(), 2, "{spawned:?}");
    assert_eq!(
        spawned[1].size,
        PtySize {
            cols: 150,
            rows: 50
        }
    );
}

#[test]
fn a_claude_session_whose_agent_never_reported_its_session_cannot_be_resumed() {
    let mut c = Claude::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client, Some("claude")).unwrap();
    client.input(&session, b"/exit\r");
    c.h.step();

    let result = c.resume(&mut client, &session);
    assert!(
        matches!(&result, Err(WireError::InvalidParams(m)) if m.contains("no agent session id")),
        "{result:?}"
    );
    assert_eq!(c.all_spawned().len(), 1);
    assert!(!c.h.run_root().join(session.as_str()).exists());
}

/// [`support::shell_default_config`] plus a profile `mine` of `kind` running `claude`, or without it for `None`.
fn mine(kind: Option<&str>) -> RawConfig {
    let mut config = support::shell_default_config();
    if let Some(kind) = kind {
        let agent = RawAgent {
            kind: Some(kind.to_owned()),
            command: Some(CLAUDE.to_owned()),
            ..RawAgent::default()
        };
        config.agents = Some([("mine".to_owned(), agent)].into());
    }
    config
}

/// How many entries of `session`'s log are `method`.
fn count(c: &Claude, session: &SessionId, method: &str) -> usize {
    let log =
        c.h.log()
            .read(LogId::Session(session.clone()), Seq::ZERO)
            .unwrap();
    log.iter()
        .filter(|e| e.frame().method() == Some(method))
        .count()
}

/// Checks a resume of `session` was refused with `InvalidParams` saying `why`, and nothing was spawned, written or
/// logged for it.
fn assert_resume_refused(c: &mut Claude, session: &SessionId, why: &str) {
    let mut client = c.h.connect();
    let result = c.resume(&mut client, session);
    assert!(
        matches!(&result, Err(WireError::InvalidParams(m)) if m.contains(why)),
        "{result:?}"
    );
    assert_eq!(c.all_spawned().len(), 1);
    assert_eq!(count(c, session, "_shepherd/agent/started"), 1);
    assert_eq!(c.h.runtime().pending_run_files(), 0);
    assert!(!c.h.run_root().join(session.as_str()).exists());
    assert!(c.h.host().sessions()[0].exited);
}

#[test]
fn resuming_a_session_whose_directory_is_gone_is_refused() {
    // The PTY would silently start in the home directory instead.
    let mut c = Claude::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("claude"), Some(&path))
        .unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    c.h.restart();
    dir.close().unwrap();
    assert_resume_refused(&mut c, &session, &format!("{path} no longer exists"));
}

#[test]
fn resuming_a_session_whose_profile_was_removed_is_refused() {
    let mut c = Claude::with_config(mine(Some("claude-code")));
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("mine"), dir.path().to_str())
        .unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    c.h.restart_with_config(mine(None));
    assert_resume_refused(&mut c, &session, "profile `mine` no longer exists");
}

#[test]
fn resuming_a_session_whose_profile_became_a_shell_is_refused() {
    let mut c = Claude::with_config(mine(Some("claude-code")));
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("mine"), dir.path().to_str())
        .unwrap();
    c.hook(&mut client, &session, "SessionStart.startup");
    c.h.restart_with_config(mine(Some("shell")));
    assert_resume_refused(&mut c, &session, "shell sessions can't be resumed");
}

#[test]
fn resuming_a_session_whose_profile_changed_kind_is_refused_and_drops_the_planned_run_files() {
    // Started as Codex; the profile now plans Claude Code's settings file, which core refuses to spawn.
    let mut c = Claude::with_config(mine(Some("codex")));
    let dir = tempfile::tempdir().unwrap();
    let mut client = c.h.connect();
    let session = c
        .new_session_in(&mut client, Some("mine"), dir.path().to_str())
        .unwrap();
    let start = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures/codex/SessionStart.startup.json");
    let sent = client.request(ClientRequest::HookFrame(Box::new(HookFrame {
        session: session.clone(),
        agent: AgentKind::Codex,
        payload: read_json(&start),
    })));
    c.h.step();
    assert_eq!(response(&client.take(), &sent), Ok(json!({})));
    c.h.restart_with_config(mine(Some("claude-code")));
    assert_resume_refused(&mut c, &session, "started as profile `mine` (codex)");
}

// ---------- the real adapters ----------

/// A real host in a tempdir whose `claude` profile runs a fake `claude` script, over a real PTY and socket.
mod real {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::Value;
    use shepherd::host::{self, RuntimeEvent, SOCKET_FILE};
    use shepherd_core::acpp::{Envelope, RequestId};
    use shepherd_core::ids::SessionId;
    use shepherd_core::wire::{
        ClientNotification, ClientRequest, HostMessage, HostNotification, InputKind, Request,
        SessionClose, SessionInput, SessionNew, Subscribe, SubscribeFrom, decode_host_message,
        encode_client_notification, encode_request,
    };

    /// A liveness bound on each read from the host, never a measurement.
    const GENEROUS: Duration = Duration::from_secs(60);

    /// Records its argv, environment and `--settings` file into `$FAKE_OUT`, says `ready`, and exits once it reads a
    /// line.
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$FAKE_OUT/args"
env > "$FAKE_OUT/env"
cp "$2" "$FAKE_OUT/settings.json"
echo ready
read line
"#;

    struct Conn {
        writer: UnixStream,
        reader: BufReader<UnixStream>,
    }

    impl Conn {
        fn open(socket: &Path) -> Self {
            let stream = UnixStream::connect(socket).unwrap();
            stream.set_read_timeout(Some(GENEROUS)).unwrap();
            Self {
                writer: stream.try_clone().unwrap(),
                reader: BufReader::new(stream),
            }
        }

        fn send(&mut self, id: i64, body: ClientRequest) {
            let line = encode_request(&Request {
                id: id.into(),
                body,
            });
            self.writer.write_all(&line).unwrap();
        }

        fn notify(&mut self, note: &ClientNotification) {
            self.writer
                .write_all(&encode_client_notification(note))
                .unwrap();
        }

        /// Reads until `session`'s PTY output so far contains `needle`.
        fn output(&mut self, session: &SessionId, needle: &[u8]) {
            let mut seen = Vec::new();
            while !seen.windows(needle.len()).any(|w| w == needle) {
                if let HostMessage::Notification(HostNotification::PtyOutput(o)) = self.next()
                    && &o.session == session
                {
                    seen.extend_from_slice(&o.bytes);
                }
            }
        }

        fn next(&mut self) -> HostMessage {
            let mut line = Vec::new();
            self.reader.read_until(b'\n', &mut line).unwrap();
            assert!(line.ends_with(b"\n"), "the host went away");
            decode_host_message(line.strip_suffix(b"\n").unwrap()).unwrap()
        }

        /// Reads until the response to `id`, returning its result.
        fn response(&mut self, id: i64) -> Value {
            let id = RequestId::from(id);
            loop {
                if let HostMessage::Response(r) = self.next()
                    && r.id == id
                {
                    return r.result.unwrap();
                }
            }
        }

        /// Reads until a log entry for `session` with `method`.
        fn entry(&mut self, session: &SessionId, method: &str) -> Envelope {
            loop {
                if let HostMessage::Notification(HostNotification::LogEntry(e)) = self.next() {
                    let env = e.envelope;
                    if env.session() == Some(session) && env.frame().method() == Some(method) {
                        return env;
                    }
                }
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn the_host_launches_claude_with_its_own_executable_as_the_hook_and_cleans_up_on_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("shepherd");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let claude = tmp.path().join("claude");
        std::fs::write(&claude, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
        host::prepare_root(&root).unwrap();
        let config = root.join(host::CONFIG_FILE);
        std::fs::write(
            &config,
            format!(
                "[agents.claude]\ncommand = {:?}\nenv = {{ FAKE_OUT = {:?} }}\n",
                claude.to_str().unwrap(),
                out.to_str().unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut host = host::open(&root, tmp.path().to_path_buf(), Some("/bin/sh".into())).unwrap();
        let tx = host.sender();
        let socket = root.join(SOCKET_FILE);
        let run_root = root.join(host::RUN_DIR);
        let client = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let mut conn = Conn::open(&socket);
                conn.send(
                    1,
                    ClientRequest::Subscribe(Subscribe {
                        from: SubscribeFrom::default(),
                    }),
                );
                conn.response(1);
                conn.send(
                    2,
                    ClientRequest::SessionNew(SessionNew {
                        profile: Some("claude".into()),
                        cwd: None,
                        branch: None,
                        size: None,
                    }),
                );
                let session = SessionId::new(conn.response(2)["session"].as_str().unwrap());
                // The script has written everything once it says so.
                conn.output(&session, b"ready");
                let dir = run_root.join(session.as_str());
                let modes = (
                    mode(&run_root),
                    mode(&dir),
                    mode(&dir.join("settings.json")),
                );
                conn.notify(&ClientNotification::SessionInput(SessionInput {
                    session: session.clone(),
                    data: b"\n".to_vec(),
                    kind: InputKind::Keys,
                }));
                conn.entry(&session, "_shepherd/agent/exited");
                let exists_after_exit = dir.exists();
                conn.send(
                    3,
                    ClientRequest::SessionClose(SessionClose {
                        session: session.clone(),
                        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
                    }),
                );
                conn.response(3);
                (session, dir, modes, exists_after_exit)
            });
            tx.send(RuntimeEvent::Shutdown);
            result
        });
        host.run();
        drop(host);
        let (session, dir, modes, exists_after_exit) = client
            .join()
            .unwrap()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));

        let settings = dir.join("settings.json");
        let args = std::fs::read_to_string(out.join("args")).unwrap();
        assert_eq!(args, format!("--settings\n{}\n", settings.display()));
        let env = std::fs::read_to_string(out.join("env")).unwrap();
        assert!(
            env.lines()
                .any(|l| l == format!("SHEPHERD_SESSION={session}")),
            "{env}"
        );
        let sock = format!("SHEPHERD_SOCK={}", root.join(SOCKET_FILE).display());
        assert!(env.lines().any(|l| l == sock), "{env}");
        let written: Value =
            serde_json::from_slice(&std::fs::read(out.join("settings.json")).unwrap()).unwrap();
        let handler = &written["hooks"]["PermissionRequest"][0]["hooks"][0];
        let exe: PathBuf = std::env::current_exe().unwrap();
        assert!(exe.is_absolute());
        assert_eq!(handler["command"].as_str(), exe.to_str());
        assert_eq!(handler["args"], serde_json::json!(["hook", "claude-code"]));
        assert_eq!(modes, (0o700, 0o700, 0o600));
        assert!(!exists_after_exit, "{} survived the exit", dir.display());
    }
}
