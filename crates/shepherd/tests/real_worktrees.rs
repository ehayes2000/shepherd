//! The finder's worktree rules on a real host (`host::open`: real git, real PTYs) whose root is reached through a
//! symlink, in a private tempdir (never `~/.shepherd`). Git runs on a throwaway repo cloned from a local seed, so the
//! fetch never leaves the machine.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use shepherd::host::{self, RuntimeEvent, SOCKET_FILE};
use shepherd_adapters::GhForge;
use shepherd_core::acpp::{Frame, RequestId, ShepherdFrame};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    ClientRequest, Hello, HostMessage, HostNotification, PROTOCOL_VERSION, Request, SessionNew,
    Subscribe, WireError, WorkspaceSelect, decode_host_message, encode_request,
};

/// A liveness bound on each read from the host, never a measurement. An answer can wait on several real git calls,
/// each of which can take minutes on a heavily loaded machine (F17), so only a host stuck this long fails the test.
const LIMIT: Duration = Duration::from_secs(600);

/// Runs git in `dir` with nothing from this process's environment but `PATH`, and no user or system config. The
/// environment must go: under a git hook (the pre-commit gate) it holds `GIT_DIR` and `GIT_INDEX_FILE`, which would
/// point these commands at the repo being committed.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A client on a raw socket: requests in, the answer to each out, log entries remembered as they pass.
struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: i64,
    seen: Vec<shepherd_core::acpp::Envelope>,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next: 0,
            seen: Vec::new(),
        }
    }

    fn message(&mut self) -> HostMessage {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line).unwrap();
        assert!(n > 0, "the host hung up");
        let message = decode_host_message(line.strip_suffix(b"\n").unwrap()).unwrap();
        if let HostMessage::Notification(HostNotification::LogEntry(entry)) = &message {
            self.seen.push(entry.envelope.clone());
        }
        message
    }

    fn request(&mut self, body: ClientRequest) -> Result<Value, WireError> {
        self.next += 1;
        let id = RequestId::from(self.next);
        let line = encode_request(&Request {
            id: id.clone(),
            body,
        });
        self.writer.write_all(&line).unwrap();
        loop {
            if let HostMessage::Response(r) = self.message()
                && r.id == id
            {
                return r.result;
            }
        }
    }

    /// The worktree path and branch session `id`'s `worktree/created` recorded, read off the log as it streams.
    fn worktree_of(&mut self, id: &SessionId) -> (String, String) {
        loop {
            let found = self.seen.iter().find_map(|env| match env.frame() {
                Frame::Shepherd(m) if env.session() == Some(id) => match m.body() {
                    ShepherdFrame::WorktreeCreated(w) => {
                        Some(Ok((w.path.clone(), w.branch.clone())))
                    }
                    ShepherdFrame::SessionFailed(f) => Some(Err(f.reason.clone())),
                    _ => None,
                },
                _ => None,
            });
            match found {
                Some(Ok(made)) => return made,
                Some(Err(why)) => panic!("the worktree wasn't made: {why}"),
                None => {
                    self.message();
                }
            }
        }
    }
}

#[test]
fn a_worktree_the_host_made_under_a_linked_root_is_no_orphan_and_cant_be_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(tmp.path()).unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = real.with_file_name("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    git(&real, &["init", "--quiet", "-b", "main", "seed"]);
    git(
        &real.join("seed"),
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    );
    git(&real, &["clone", "--quiet", "seed", "app"]);
    let app = real.join("app");
    // The root, and so the worktrees under it, is reached through the symlink.
    let root = alias.join("shepherd");

    // Its sessions have branches, so it polls their PRs: through an empty search path, never the real `gh`.
    let no_gh = real.join("no-gh");
    std::fs::create_dir(&no_gh).unwrap();
    let forge = GhForge::new().with_search_path(&no_gh);
    let mut host =
        host::open_with_forge(&root, PathBuf::from("/"), Some("/bin/sh".into()), forge).unwrap();
    let tx = host.sender();
    let socket = root.join(SOCKET_FILE);
    let client = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(move || {
            let mut c = Client::connect(&socket);
            let hello = Hello {
                version: PROTOCOL_VERSION,
                machine: MachineId::new("laptop"),
                cwd: None,
                build: None,
            };
            c.request(ClientRequest::Hello(hello)).unwrap();
            let select = WorkspaceSelect {
                path: app.to_str().unwrap().to_owned(),
            };
            c.request(ClientRequest::WorkspaceSelect(select)).unwrap();
            c.request(ClientRequest::Subscribe(Subscribe::default()))
                .unwrap();
            let new = SessionNew {
                profile: Some("shell".to_owned()),
                ..SessionNew::default()
            };
            let made = c.request(ClientRequest::SessionNew(new)).unwrap();
            let id = SessionId::new(made["session"].as_str().unwrap());
            let (path, branch) = c.worktree_of(&id);
            let resolved = std::fs::canonicalize(&path).unwrap();

            let listed = c
                .request(ClientRequest::WorktreeList(Default::default()))
                .unwrap();
            assert_eq!(listed["worktrees"], serde_json::json!([]), "{path}");

            let adopt = SessionNew {
                profile: Some("shell".to_owned()),
                cwd: Some(resolved.to_str().unwrap().to_owned()),
                branch: Some(branch),
                size: None,
            };
            let refused = c.request(ClientRequest::SessionNew(adopt));
            assert!(
                matches!(&refused, Err(WireError::InvalidParams(why)) if why.contains("already in use")),
                "{refused:?}"
            );
        });
        tx.send(RuntimeEvent::Shutdown);
        result
    });
    host.run();
    client
        .join()
        .unwrap()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
}
