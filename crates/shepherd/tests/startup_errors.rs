//! The `shepherd` binary end to end: a host that can't start says why on the client's stderr, and a host that
//! starts hands its stderr off, so the launcher knows it started. Each test has its own temp `HOME`, never the real
//! `~/.shepherd`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");
/// Generous liveness bound: the client waits up to the launcher's own bound for the host.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A child process killed and reaped when dropped, so a failed test leaves nothing running.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The pids holding `<home>/.shepherd`'s socket or event-log lock open: a host serving there. Found by exact path
/// with `lsof`, never by process name, since every worktree's test host has the same name.
fn host_pids(home: &Path) -> Vec<u32> {
    let root = home.join(".shepherd");
    let paths: Vec<PathBuf> = [root.join("host.sock"), root.join("lock")]
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if paths.is_empty() {
        return Vec::new();
    }
    let Ok(out) = Command::new("lsof").arg("-t").args(&paths).output() else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .filter(|&pid| pid != std::process::id())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Sends `signal` to exactly `pid`, then waits (bounded) for it to go away. Returns whether it did.
fn stop(pid: u32, signal: &str) -> bool {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A temp `HOME`. When dropped, on failure too, stops any host serving in it: a host the client launched is
/// detached, so nothing else would.
struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        for pid in host_pids(self.path()) {
            if !stop(pid, "-TERM") {
                stop(pid, "-KILL");
            }
        }
    }
}

#[test]
fn a_host_that_cannot_start_tells_the_client_why_and_the_client_exits_non_zero() {
    let home = Home::new();
    let root = home.path().join(".shepherd");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    let stderr_file = home.path().join("stderr");

    // The client needs a terminal; its stderr goes to a file so it can be told apart from what it draws.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args([
        "-c",
        r#"exec "$0" 2>"$1""#,
        SHEPHERD,
        stderr_file.to_str().unwrap(),
    ]);
    cmd.env("HOME", home.path());
    let mut child = pty.slave.spawn_command(cmd).unwrap();
    drop(pty.slave);
    let mut killer = child.clone_killer();
    let mut reader = pty.master.try_clone_reader().unwrap();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = match rx.recv_timeout(TIMEOUT) {
        Ok(status) => status.unwrap(),
        Err(_) => {
            let _ = killer.kill();
            panic!("shepherd didn't exit");
        }
    };
    drop(pty.master);

    let stderr = fs::read_to_string(&stderr_file).unwrap();
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!("chmod 700 {}", root.display())),
        "{stderr}"
    );
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn a_host_that_starts_closes_its_stderr_and_keeps_running() {
    let home = Home::new();
    let mut host = Reaped(
        Command::new(SHEPHERD)
            .arg("host")
            .env("HOME", home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = host.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = tx.send(stderr.read_to_end(&mut out).map(|_| out));
    });
    let written = rx
        .recv_timeout(TIMEOUT)
        .expect("the host never closed its stderr")
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&written), "");
    assert!(
        home.path().join(".shepherd/host.sock").exists(),
        "closed before serving"
    );
    assert!(host.0.try_wait().unwrap().is_none(), "the host exited");
    // What `Home` stops on drop finds exactly this host.
    assert_eq!(host_pids(home.path()), [host.0.id()]);
}
