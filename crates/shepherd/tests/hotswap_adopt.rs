//! A host's next image adopting what the previous one handed off (T36b, decision 0018), in one process on the real
//! adapters: the hand-off is done as for an exec, the old image is forgotten (an exec never runs its destructors),
//! and `open_adopted` takes over. A temp root, never the real `~/.shepherd`. Timeouts are liveness bounds only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;
use shepherd::host::{self, CarriedSession, RealHost, SwapPlan, swap};
use shepherd_adapters::handoff::is_inherited;
use shepherd_core::acpp::RequestId;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{Pty, PtyHandoff};
use shepherd_core::wire::{
    ClientRequest, Hello, PROTOCOL_VERSION, Request, SessionNew, encode_request,
};

const LIVENESS: Duration = Duration::from_secs(60);

/// A raw client on the host socket: lines read on a thread of their own, `None` once the host closes it.
struct Client {
    stream: UnixStream,
    lines: Receiver<Option<Value>>,
    next: i64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        let reader = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).split(b'\n') {
                let Ok(line) = line else { break };
                let _ = tx.send(Some(serde_json::from_slice(&line).unwrap()));
            }
            let _ = tx.send(None);
        });
        Self {
            stream,
            lines,
            next: 0,
        }
    }

    fn request(&mut self, body: ClientRequest) -> RequestId {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.stream
            .write_all(&encode_request(&Request {
                id: id.clone(),
                body,
            }))
            .unwrap();
        id
    }

    fn hello(&mut self) -> RequestId {
        self.request(ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("adopt"),
            cwd: None,
            build: None,
        }))
    }

    /// Runs `host` until the answer to `id` arrives, and returns its `result`.
    fn answer(&mut self, host: &mut RealHost, id: &RequestId) -> Value {
        let id = serde_json::to_value(id).unwrap();
        let deadline = Instant::now() + LIVENESS;
        while Instant::now() < deadline {
            host.run_next(Duration::from_millis(20));
            while let Ok(line) = self.lines.try_recv() {
                let line = line.expect("the host closed the connection");
                if line["id"] == id {
                    return line["result"].clone();
                }
            }
        }
        panic!("no answer within {LIVENESS:?}");
    }

    /// Whether the host closed this connection, within the liveness bound.
    fn closed(&self) -> bool {
        let deadline = Instant::now() + LIVENESS;
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if line.is_none() {
                return true;
            }
        }
        false
    }
}

fn write_config(root: &Path, contents: &[u8]) {
    let path = root.join(host::CONFIG_FILE);
    let _ = fs::remove_file(&path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(contents).unwrap();
}

#[test]
fn a_new_image_keeps_its_sessions_when_the_config_broke_meanwhile_and_takes_over_cleanly() {
    let home = tempfile::Builder::new()
        .prefix("shad")
        .tempdir_in("/tmp")
        .unwrap();
    let root = home.path().join(".shepherd");
    host::prepare_root(&root).unwrap();
    write_config(&root, b"[defaults]\nagent = \"shell\"\n");
    let socket = root.join(host::SOCKET_FILE);
    let mut old = host::open(&root, PathBuf::from("/"), Some("/bin/sh".into())).unwrap();

    // A running shell session.
    let mut a = Client::connect(&socket);
    let hello = a.hello();
    a.answer(&mut old, &hello);
    let new = a.request(ClientRequest::SessionNew(SessionNew::default()));
    let session = SessionId::new(a.answer(&mut old, &new)["session"].as_str().unwrap());

    // The hand-off, as for an exec.
    let tokens = old.pty_mut().export().unwrap();
    let plan = SwapPlan {
        sessions: tokens
            .into_iter()
            .map(|(id, token)| CarriedSession {
                id,
                cols: 80,
                rows: 24,
                snapshot: Vec::new(),
                token,
            })
            .collect(),
        translators: Default::default(),
    };
    let exe = PathBuf::from("/opt/shepherd/bin/shepherd");
    let handed = swap::hand_off(&mut old, plan, &root, &exe).unwrap();
    let carried = handed.file.fds();

    let mode = fs::metadata(&handed.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the handoff file is readable by others");
    for &fd in &carried {
        assert!(
            is_inherited(fd).unwrap(),
            "fd {fd} wouldn't survive the exec"
        );
    }
    assert!(a.closed(), "the old image's connection stayed open");
    // Connected after the old image closed its conns: waits in the backlog for the new one.
    let mut b = Client::connect(&socket);
    let hello = b.hello();
    // Broken between the preflight and the new image.
    write_config(&root, b"not = [toml");
    std::mem::forget(old);

    let mut new = host::open_adopted(
        &root,
        PathBuf::from("/"),
        Some("/bin/sh".into()),
        &handed.path,
    )
    .expect("the new image took over");

    for &fd in &carried {
        assert!(
            !is_inherited(fd).unwrap(),
            "fd {fd} would leak into the new image's children"
        );
    }
    assert!(!handed.path.exists(), "the handoff file was left behind");
    let answer = b.answer(&mut new, &hello);
    let warning = answer["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("config"), "{answer}");
    assert_eq!(
        new.settings().shepherd,
        exe,
        "the startup path isn't the one execed"
    );
    let listed = new.host().sessions();
    let listed = listed.iter().find(|s| s.id == session).expect("listed");
    assert!(!listed.exited, "the session was recorded as exited");
    new.pty_mut().write(&session, b"echo still\n").unwrap();
}
