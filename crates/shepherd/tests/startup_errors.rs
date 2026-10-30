//! The `shepherd` binary end to end: a host that can't start says why on the client's stderr, and a host that
//! starts hands its stderr off, so the launcher knows it started. Each test has its own temp `HOME`, never the real
//! `~/.shepherd`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

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

#[test]
fn a_host_that_cannot_start_tells_the_client_why_and_the_client_exits_non_zero() {
    let home = tempfile::tempdir().unwrap();
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
    let home = tempfile::tempdir().unwrap();
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
}
