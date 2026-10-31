//! `shepherd peers` and `shepherd unpair` (T30) with the built binary, against a host in a temp `HOME` (never the real
//! `~/.shepherd`) that never paired, so it never goes online: these make no network calls. Pairing itself is covered
//! on fakes (`pairing.rs`) and on localhost endpoints (the `Peers` contract in `shepherd-adapters`). Timeouts are
//! liveness bounds only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");
const LIVENESS: Duration = Duration::from_secs(60);

/// The host, stopped when dropped (on failure too): exactly this pid, never by name.
struct Running {
    host: Child,
    home: tempfile::TempDir,
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
    }
}

/// Starts `shepherd host` in a fresh temp `HOME` and waits until it serves (it releases its stderr then).
fn start_host() -> Running {
    // Under /tmp: the socket path must stay under the OS limit.
    let home = tempfile::Builder::new()
        .prefix("shpe")
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
    Running { host, home }
}

fn shepherd(home: &Path, args: &[&str]) -> Output {
    Command::new(SHEPHERD)
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn peers_on_a_machine_that_never_paired_lists_none_and_unpair_says_there_is_no_such_peer() {
    let running = start_host();
    let home = running.home.path();

    let out = shepherd(home, &["peers"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "No paired machines. Pair one with `shepherd pair`.\n"
    );

    let out = shepherd(home, &["unpair", "laptop"]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no trusted machine has that name or id"),
        "{stderr}"
    );

    let out = shepherd(home, &["join"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: shepherd join <ticket>"));

    // It never went online, so it learned no peer's address.
    assert!(!home.join(".shepherd").join("peer-addrs.json").exists());
}
