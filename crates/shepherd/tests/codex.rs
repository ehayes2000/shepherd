//! Codex sessions: the host launches `codex` from its profile with shepherd's hooks (and their trust state) in one
//! `-c hooks=...` config override, and the session's status follows the hooks. On the fake ports with a fake `codex`,
//! plus one real-adapter run of a fake `codex` script. Never the real `codex`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use shepherd::hook::{self, HookError};
use shepherd_core::agents::codex;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{PtySize, SpawnSpec};
use shepherd_core::session::{Activity, SessionState};
use shepherd_core::wire::{ClientRequest, SessionNew, WireError};
use shepherd_testkit::fakes::link::FakeNetwork;
use shepherd_testkit::fakes::pty::ProgramInput;
use support::{FakeClient, HOME, HostHarness, Program, SHEPHERD_EXE, SOCKET, entries, response};

/// The command of the built-in `codex` profile.
const CODEX: &str = "codex";
/// A liveness bound for hook threads, never reached when the host is stepped.
const GENEROUS: Duration = Duration::from_secs(60);

type Hooks = Arc<Mutex<Vec<JoinHandle<Result<(), HookError>>>>>;
type Spawned = Arc<Mutex<Vec<SpawnSpec>>>;

/// The `hooks` table from a `-c hooks=<value>` argument, parsed as Codex parses `-c` values: the text after `=` is
/// the TOML value of a sentinel key (`codex-rs/utils/cli/src/config_override.rs`, `parse_toml_value`).
fn hooks_override(args: &[String]) -> Option<toml::Table> {
    let at = args.iter().position(|a| a == "-c")?;
    let value = args.get(at + 1)?.strip_prefix("hooks=")?;
    let mut parsed: toml::Table = toml::from_str(&format!("_x_ = {value}")).ok()?;
    match parsed.remove("_x_")? {
        toml::Value::Table(t) => Some(t),
        _ => None,
    }
}

/// The program and arguments of a hook command as `$SHELL -lc` would split it, for the commands shepherd writes: a
/// single-quoted path, then plain words.
fn split_command(command: &str) -> Option<(String, Vec<String>)> {
    let rest = command.strip_prefix('\'')?;
    let (program, args) = rest.split_once('\'')?;
    Some((
        program.to_owned(),
        args.split_whitespace().map(str::to_owned).collect(),
    ))
}

/// A fake `codex`. It records how it was spawned. Typed a hook payload (one JSON line), it does what Codex does with
/// the config override it was given: looks up the handler for the payload's `hook_event_name` in `-c hooks=...`,
/// skips it unless `hooks.state` trusts it by Codex's key and hash, and runs it with the payload on stdin and its own
/// environment. Running is simulated: a command whose program is [`SHEPHERD_EXE`] is `shepherd`, so the fake calls
/// `shepherd hook <agent>` in-process, on its own thread like a separate hook process.
fn fake_codex(net: &FakeNetwork, hooks: &Hooks, spawned: &Spawned) -> Program {
    let (net, hooks, spawned) = (net.clone(), hooks.clone(), spawned.clone());
    Arc::new(move |input, p| {
        let spec = p.spec().clone();
        let bytes = match input {
            ProgramInput::Start => return spawned.lock().unwrap().push(spec),
            ProgramInput::Stdin(bytes) => bytes,
            ProgramInput::Resized => return,
        };
        let Ok(payload) = serde_json::from_slice::<Value>(bytes.trim_ascii_end()) else {
            return;
        };
        let Some(config) = hooks_override(&spec.args) else {
            return;
        };
        let event = payload["hook_event_name"].as_str().unwrap_or_default();
        let Some(command) = config.get(event).and_then(|groups| {
            groups
                .get(0)?
                .get("hooks")?
                .get(0)?
                .get("command")?
                .as_str()
        }) else {
            return;
        };
        let trusted = codex::hook_trust_key(event).and_then(|key| {
            config
                .get("state")?
                .get(&key)?
                .get("trusted_hash")?
                .as_str()
                .map(str::to_owned)
        });
        if trusted.is_none() || trusted != codex::hook_trust_hash(event, command) {
            return;
        }
        let Some((program, args)) = split_command(command) else {
            return;
        };
        let (SHEPHERD_EXE, [sub, agent]) = (program.as_str(), &args[..]) else {
            return;
        };
        assert_eq!(sub, "hook");
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

/// A harness whose `codex` command runs [`fake_codex`].
struct Codex {
    h: HostHarness,
    hooks: Hooks,
    spawned: Spawned,
}

impl Codex {
    fn start() -> Self {
        let net = FakeNetwork::new();
        let hooks = Hooks::default();
        let spawned = Spawned::default();
        let h = HostHarness::builder()
            .network(net.clone())
            .config(support::shell_default_config())
            .program(CODEX, fake_codex(&net, &hooks, &spawned))
            .start();
        Self { h, hooks, spawned }
    }

    fn new_session(&mut self, client: &mut FakeClient) -> Result<SessionId, WireError> {
        let sent = client.request(ClientRequest::SessionNew(SessionNew {
            profile: Some("codex".to_owned()),
            cwd: None,
        }));
        self.h.step();
        response(&client.take(), &sent)
            .map(|r| SessionId::new(r["session"].as_str().expect("session id")))
    }

    fn spawned(&self) -> SpawnSpec {
        let spawned = self.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1, "{spawned:?}");
        spawned[0].clone()
    }

    /// Steps the host until every hook the fake `codex` ran has its answer.
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

#[test]
fn a_codex_session_runs_codex_with_the_hooks_override_and_shepherds_variables_and_no_run_files() {
    let mut c = Codex::start();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client).unwrap();

    let spec = c.spawned();
    assert_eq!(spec.command, CODEX);
    assert_eq!(spec.cwd, PathBuf::from(HOME));
    assert_eq!(spec.size, PtySize { cols: 80, rows: 24 });
    assert_eq!(env_var(&spec, "SHEPHERD_SESSION"), Some(session.as_str()));
    assert_eq!(env_var(&spec, "SHEPHERD_SOCK"), Some(SOCKET));
    assert_eq!(spec.args.len(), 2, "{:?}", spec.args);
    let hooks = hooks_override(&spec.args).expect("a -c hooks=... override");
    let command = format!("'{SHEPHERD_EXE}' hook codex");
    assert_eq!(
        hooks["Stop"][0]["hooks"][0]["command"].as_str(),
        Some(command.as_str())
    );
    assert!(!c.h.run_root().join(session.as_str()).exists());
}

/// What the sidebar shows for an activity, without the details.
fn status(activity: &Activity) -> &'static str {
    match activity {
        Activity::Working { .. } => "Working",
        Activity::NeedsPermission { .. } => "NeedsPermission",
        Activity::Idle => "Idle",
        Activity::Starting => "Starting",
        Activity::Exited { .. } => "Exited",
        other => panic!("unexpected {other:?}"),
    }
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures/codex")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn a_codex_sessions_status_follows_its_hooks_through_a_permission_prompt_and_an_interrupt() {
    let mut c = Codex::start();
    let mut watcher = c.h.connect();
    watcher.subscribe_all();
    c.h.step();
    let mut client = c.h.connect();
    let session = c.new_session(&mut client).unwrap();

    let mut state = SessionState::default();
    let mut seen = vec![];
    for hook in [
        "SessionStart.startup",
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "PermissionRequest.Bash",
        "PostToolUse.Bash",
        "Stop",
        "UserPromptSubmit",
        "PreToolUse.Bash",
        "Interrupt",
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
    assert_eq!(
        seen,
        [
            "Starting",
            "Working",
            "NeedsPermission",
            "Working",
            "Idle",
            "Working",
            "Idle"
        ]
    );
}

// ---------- the real adapters ----------

/// A real host in a tempdir whose `codex` profile runs a fake `codex` script, over a real PTY and socket.
mod real {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    use serde_json::Value;
    use shepherd::host::{self, RuntimeEvent, SOCKET_FILE};
    use shepherd_core::acpp::RequestId;
    use shepherd_core::ids::SessionId;
    use shepherd_core::wire::{
        ClientRequest, HostMessage, HostNotification, Request, SessionNew, Subscribe,
        SubscribeFrom, decode_host_message, encode_request,
    };

    /// A liveness bound on each read from the host, never a measurement.
    const GENEROUS: Duration = Duration::from_secs(60);

    /// Records its argv and environment into `$FAKE_OUT`, then exits.
    const FAKE_CODEX: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$FAKE_OUT/args"
env > "$FAKE_OUT/env"
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

        fn next(&mut self) -> HostMessage {
            let mut line = Vec::new();
            self.reader.read_until(b'\n', &mut line).unwrap();
            assert!(line.ends_with(b"\n"), "the host went away");
            decode_host_message(line.strip_suffix(b"\n").unwrap()).unwrap()
        }

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

        fn wait_exited(&mut self, session: &SessionId) {
            loop {
                if let HostMessage::Notification(HostNotification::LogEntry(e)) = self.next()
                    && e.envelope.session() == Some(session)
                    && e.envelope.frame().method() == Some("_shepherd/agent/exited")
                {
                    return;
                }
            }
        }
    }

    #[test]
    fn the_host_launches_codex_with_its_own_executable_as_the_quoted_hook_command() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("shepherd");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let codex = tmp.path().join("codex");
        std::fs::write(&codex, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        host::prepare_root(&root).unwrap();
        let config = root.join(host::CONFIG_FILE);
        std::fs::write(
            &config,
            format!(
                "[agents.codex]\ncommand = {:?}\nargs = [\"--model\", \"o3\"]\nenv = {{ FAKE_OUT = {:?} }}\n",
                codex.to_str().unwrap(),
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
                        profile: Some("codex".into()),
                        cwd: None,
                    }),
                );
                let session = SessionId::new(conn.response(2)["session"].as_str().unwrap());
                conn.wait_exited(&session);
                let run_dir_made = run_root.join(session.as_str()).exists();
                (session, run_dir_made)
            });
            tx.send(RuntimeEvent::Shutdown);
            result
        });
        host.run();
        drop(host);
        let (session, run_dir_made) = client
            .join()
            .unwrap()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));

        let args = std::fs::read_to_string(out.join("args")).unwrap();
        let args: Vec<String> = args.lines().map(str::to_owned).collect();
        assert_eq!(args.len(), 4, "{args:?}");
        assert_eq!(args[2..], ["--model", "o3"]);
        let hooks = super::hooks_override(&args).expect("a -c hooks=... override");
        let exe = std::env::current_exe().unwrap();
        let command = format!("'{}' hook codex", exe.display());
        let handler = &hooks["PermissionRequest"][0]["hooks"][0];
        assert_eq!(handler["command"].as_str(), Some(command.as_str()));
        let key = shepherd_core::agents::codex::hook_trust_key("PermissionRequest").unwrap();
        assert_eq!(
            hooks["state"][&key]["trusted_hash"]
                .as_str()
                .map(str::to_owned),
            shepherd_core::agents::codex::hook_trust_hash("PermissionRequest", &command)
        );
        let env = std::fs::read_to_string(out.join("env")).unwrap();
        assert!(
            env.lines()
                .any(|l| l == format!("SHEPHERD_SESSION={session}")),
            "{env}"
        );
        assert!(!run_dir_made, "a codex session needs no run directory");
    }
}
