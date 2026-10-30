//! The host updating itself in place, end to end with the built binary (T36b, decision 0018): a temp `HOME` (never
//! the real `~/.shepherd`), a shell session, and `_shepherd/host/upgrade { force: true }`, which re-execs the same
//! binary. Timeouts are liveness bounds only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use shepherd_core::acpp::{Envelope, Frame, RequestId, Seq, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    ClientNotification, ClientRequest, Hello, HostMessage, HostNotification, HostUpgrade, Method,
    PROTOCOL_VERSION, Request, SessionInput, SessionNew, Subscribe, SubscribeFrom, UpgradeAnswer,
    decode_host_message, encode_client_notification, encode_request,
};

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");
const LIVENESS: Duration = Duration::from_secs(60);

/// The host and the session's shell, stopped when dropped (on failure too): exactly these pids, never by name.
struct Running {
    host: Child,
    shell: Option<u32>,
    _home: tempfile::TempDir,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.host.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.host.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::yield_now();
        }
        let _ = self.host.kill();
        let _ = self.host.wait();
        if let Some(shell) = self.shell {
            // Its process group: the shell is a session leader.
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{shell}")])
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts `shepherd host` in a fresh temp `HOME` whose config runs a plain shell, and waits until it serves: it
/// releases its stderr then (the launcher's handshake).
fn start_host() -> (Running, PathBuf) {
    // Under /tmp: the socket path must stay under the OS limit.
    let home = tempfile::Builder::new()
        .prefix("shsw")
        .tempdir_in("/tmp")
        .unwrap();
    let root = home.path().join(".shepherd");
    shepherd::host::prepare_root(&root).unwrap();
    let mut config = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(shepherd::host::CONFIG_FILE))
        .unwrap();
    config
        .write_all(b"[defaults]\nagent = \"shell\"\n")
        .unwrap();
    let mut host = Command::new(SHEPHERD)
        .arg("host")
        .env("HOME", home.path())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = host.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stderr.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let said = rx.recv_timeout(LIVENESS).expect("the host started");
    assert!(said.is_empty(), "the host failed to start: {said}");
    let running = Running {
        host,
        shell: None,
        _home: home,
    };
    (running, root)
}

/// A client on the host socket, reading lines on a thread of its own.
struct Client {
    stream: UnixStream,
    lines: Receiver<Option<Vec<u8>>>,
    next: i64,
    pending: BTreeMap<RequestId, Method>,
}

/// What a client has read.
#[derive(Default)]
struct Seen {
    entries: Vec<Envelope>,
    output: Vec<u8>,
    answers: Vec<(Method, serde_json::Value)>,
    closed: bool,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to the host");
        let reader = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).split(b'\n') {
                match line {
                    Ok(line) => {
                        if tx.send(Some(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(None);
        });
        let mut client = Self {
            stream,
            lines,
            next: 0,
            pending: BTreeMap::new(),
        };
        client.request(ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("e2e"),
            cwd: None,
            build: None,
        }));
        client
    }

    fn request(&mut self, body: ClientRequest) {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.pending.insert(id.clone(), body.method());
        let line = encode_request(&Request { id, body });
        self.stream.write_all(&line).unwrap();
    }

    fn input(&mut self, session: &SessionId, data: &[u8]) {
        let note = ClientNotification::SessionInput(SessionInput {
            session: session.clone(),
            data: data.to_vec(),
            kind: Default::default(),
        });
        self.stream
            .write_all(&encode_client_notification(&note))
            .unwrap();
    }

    /// Reads into `seen` until `done` says so, or panics at the liveness bound.
    fn read_until(&mut self, seen: &mut Seen, mut done: impl FnMut(&Seen) -> bool) {
        let deadline = Instant::now() + LIVENESS;
        while !done(seen) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(Some(line)) => match decode_host_message(&line).expect("a valid message") {
                    HostMessage::Response(r) => {
                        let method = self.pending.remove(&r.id).expect("an answer to a request");
                        let result = r.result.unwrap_or_else(|e| panic!("{method:?}: {e}"));
                        seen.answers.push((method, result));
                    }
                    HostMessage::Notification(HostNotification::LogEntry(e)) => {
                        seen.entries.push(e.envelope)
                    }
                    HostMessage::Notification(HostNotification::PtyOutput(o)) => {
                        seen.output.extend(o.bytes)
                    }
                },
                Ok(None) => seen.closed = true,
                Err(_) => panic!("nothing more from the host within {LIVENESS:?}"),
            }
            if seen.closed && !done(seen) {
                panic!("the host closed the connection");
            }
        }
    }
}

fn answered(seen: &Seen, method: Method) -> Option<&serde_json::Value> {
    seen.answers
        .iter()
        .find(|(m, _)| *m == method)
        .map(|(_, v)| v)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The last seq of each log among `entries`, as a subscribe resumes from.
fn last_seqs(entries: &[Envelope]) -> SubscribeFrom {
    let mut from = SubscribeFrom::default();
    for e in entries {
        match e.session() {
            None => from.host = Some(e.seq()),
            Some(s) => {
                from.sessions.insert(s.clone(), e.seq());
            }
        }
    }
    from
}

/// Per log, the seqs among `entries`, in the order received.
fn seqs_by_log(entries: &[Envelope]) -> BTreeMap<Option<SessionId>, Vec<u64>> {
    let mut seqs: BTreeMap<Option<SessionId>, Vec<u64>> = BTreeMap::new();
    for e in entries {
        seqs.entry(e.session().cloned())
            .or_default()
            .push(e.seq().get());
    }
    seqs
}

fn is_exit(e: &Envelope) -> bool {
    matches!(e.frame(), Frame::Shepherd(m) if matches!(m.body(), ShepherdFrame::AgentExited(_)))
}

#[test]
fn a_forced_upgrade_re_execs_the_host_in_place_and_the_shell_keeps_running() {
    let (mut running, root) = start_host();
    let socket = root.join(shepherd::host::SOCKET_FILE);
    let host_pid = running.host.id();

    // A shell session, and its pid.
    let mut a = Client::connect(&socket);
    a.request(ClientRequest::Subscribe(Subscribe::default()));
    a.request(ClientRequest::SessionNew(SessionNew::default()));
    let mut seen = Seen::default();
    a.read_until(&mut seen, |s| answered(s, Method::SessionNew).is_some());
    let session = SessionId::new(
        answered(&seen, Method::SessionNew).unwrap()["session"]
            .as_str()
            .unwrap(),
    );
    a.input(&session, b"echo pid=$$\n");
    a.read_until(&mut seen, |s| {
        text(&s.output)
            .split("pid=")
            .any(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    });
    let shell: u32 = text(&seen.output)
        .split("pid=")
        .find_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap();
    running.shell = Some(shell);
    let before = seen.entries.clone();

    // The swap: answered, then the connection closes.
    a.request(ClientRequest::HostUpgrade(HostUpgrade { force: true }));
    a.read_until(&mut seen, |s| s.closed);
    let answer: UpgradeAnswer =
        serde_json::from_value(answered(&seen, Method::HostUpgrade).unwrap().clone()).unwrap();
    assert_eq!(answer, UpgradeAnswer::Accepted);

    // A reconnecting client picks up where it left off: nothing missing, nothing twice.
    let mut b = Client::connect(&socket);
    b.request(ClientRequest::Subscribe(Subscribe {
        from: last_seqs(&before),
    }));
    let mut resumed = Seen::default();
    b.read_until(&mut resumed, |s| answered(s, Method::Subscribe).is_some());
    assert_eq!(running.host.try_wait().unwrap(), None, "the host exited");
    assert_eq!(running.host.id(), host_pid);
    let mut all = before.clone();
    all.extend(resumed.entries.iter().cloned());
    for (log, seqs) in seqs_by_log(&all) {
        let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
        assert_eq!(seqs, expected, "log {log:?}");
    }

    // The same shell, alive and answering.
    assert!(alive(shell), "the shell {shell} died");
    b.input(&session, b"echo after-$((40+2))\n");
    b.read_until(&mut resumed, |s| text(&s.output).contains("after-42"));

    // Its log says it never exited, and the host says it took over.
    let mut c = Client::connect(&socket);
    c.request(ClientRequest::Subscribe(Subscribe {
        from: SubscribeFrom {
            host: None,
            sessions: BTreeMap::from([(session.clone(), Seq::ZERO)]),
        },
    }));
    let mut full = Seen::default();
    c.read_until(&mut full, |s| answered(s, Method::Subscribe).is_some());
    assert!(
        !full
            .entries
            .iter()
            .any(|e| e.session() == Some(&session) && is_exit(e)),
        "an exit was recorded for {session}"
    );
    let logged = fs::read_to_string(root.join(shepherd::host::LOG_FILE)).unwrap();
    assert!(
        logged.contains("took over from the previous host image"),
        "{logged}"
    );
    assert!(
        !shepherd::host::swap::handoff_path(&root, host_pid).exists(),
        "the handoff file was left behind"
    );
}

#[test]
fn an_exec_that_fails_restores_cloexec_takes_the_listener_back_and_resumes_the_sessions() {
    use shepherd_adapters::handoff::is_inherited;
    use shepherd_core::ports::{Pty, PtyError, PtyHandoff, PtySize, SpawnSpec};

    let home = tempfile::Builder::new()
        .prefix("shsx")
        .tempdir_in("/tmp")
        .unwrap();
    let root = home.path().join(".shepherd");
    let mut host = shepherd::host::open(&root, PathBuf::from("/"), Some("/bin/sh".into())).unwrap();
    let session = SessionId::new("s_exec");
    let spec = SpawnSpec {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "cat".into()],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        size: PtySize { cols: 80, rows: 24 },
    };
    host.pty_mut().spawn(session.clone(), spec).unwrap();
    let tokens = host.pty_mut().export().unwrap();
    let carried: Vec<i32> = tokens.iter().flat_map(|(_, t)| t.fds()).collect();
    let plan = shepherd::host::SwapPlan {
        sessions: tokens
            .into_iter()
            .map(|(id, token)| shepherd::host::CarriedSession {
                id,
                cols: 80,
                rows: 24,
                snapshot: Vec::new(),
                token,
            })
            .collect(),
        translators: Default::default(),
    };
    let lock = host.log().lock_fd();

    let why = shepherd::host::swap::exec(&mut host, plan, &root, &home.path().join("missing"));

    assert!(why.contains("No such file or directory"), "{why}");
    assert!(!is_inherited(lock).unwrap());
    for fd in carried {
        assert!(!is_inherited(fd).unwrap(), "fd {fd} left inheritable");
    }
    assert!(!shepherd::host::swap::handoff_path(&root, std::process::id()).exists());
    // Still serving: a new client is accepted and answered.
    let mut client = UnixStream::connect(root.join(shepherd::host::SOCKET_FILE)).unwrap();
    client
        .write_all(&encode_request(&Request {
            id: RequestId::from(1),
            body: ClientRequest::Hello(Hello {
                version: PROTOCOL_VERSION,
                machine: MachineId::new("e2e"),
                cwd: None,
                build: None,
            }),
        }))
        .unwrap();
    assert!(host.run_next(LIVENESS), "the host accepted no one");
    while host.run_next(Duration::from_millis(200)) {}
    client.set_read_timeout(Some(LIVENESS)).unwrap();
    let mut line = Vec::new();
    BufReader::new(&client)
        .read_until(b'\n', &mut line)
        .unwrap();
    assert!(text(&line).contains("\"result\""), "{}", text(&line));
    // And the session is served again.
    assert_eq!(
        host.pty_mut().write(&session, b"x\n"),
        Err(PtyError::UnknownSession)
    );
    host.swap_failed(why);
    host.pty_mut().write(&session, b"x\n").unwrap();
    host.pty_mut().kill(&session).unwrap();
}

#[test]
fn handoff_check_prints_one_json_line_with_its_build_and_handoff_versions() {
    use shepherd_core::host::upgrade::{HANDOFF_VERSION, HandoffCheck};

    let out = Command::new(SHEPHERD)
        .arg("handoff-check")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    let stdout = text(&out.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let check: HandoffCheck = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(check.handoff_versions, vec![HANDOFF_VERSION]);
    let built = shepherd::build::build_id(Path::new(SHEPHERD)).unwrap();
    assert_eq!(check.build, built);
}
