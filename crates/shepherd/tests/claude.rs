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
use shepherd_core::config::RawConfig;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LogId, PtySize, SpawnSpec};
use shepherd_core::session::{Activity, SessionState};
use shepherd_core::wire::{ClientRequest, SessionClose, SessionNew, WireError};
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
/// own thread like a separate hook process. Any other handler, or none, does nothing.
fn fake_claude(net: &FakeNetwork, hooks: &Hooks, spawned: &Spawned) -> Program {
    let (net, hooks, spawned) = (net.clone(), hooks.clone(), spawned.clone());
    Arc::new(move |input, p| {
        let spec = p.spec().clone();
        let bytes = match input {
            ProgramInput::Start => return spawned.lock().unwrap().push(spec),
            ProgramInput::Stdin(b"/exit\r") => return p.exit(0),
            ProgramInput::Stdin(bytes) => bytes,
            ProgramInput::Resized => return,
        };
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
        let builder = HostHarness::builder()
            .network(net.clone())
            .config(support::shell_default_config())
            .program(CLAUDE, fake_claude(&net, &hooks, &spawned));
        let h = setup(builder).start();
        Self { h, hooks, spawned }
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
    assert_eq!(c.spawned().cwd, dir.path());
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
        ClientNotification, ClientRequest, HostMessage, HostNotification, Request, SessionClose,
        SessionInput, SessionNew, Subscribe, SubscribeFrom, decode_host_message,
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
                }));
                conn.entry(&session, "_shepherd/agent/exited");
                let exists_after_exit = dir.exists();
                conn.send(
                    3,
                    ClientRequest::SessionClose(SessionClose {
                        session: session.clone(),
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
