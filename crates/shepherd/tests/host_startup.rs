//! `shepherd host` startup and shutdown on the real adapters, in a private tempdir (never `~/.shepherd`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use shepherd::host::{self, HostStartError, LOG_FILE, RuntimeEvent, SOCKET_FILE};
use shepherd_adapters::JsonlEventLog;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn open(root: &Path) -> Result<host::RealHost, HostStartError> {
    host::open(root, PathBuf::from("/"), Some("/bin/sh".into()))
}

#[test]
fn the_root_is_created_private() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let _host = open(&root).unwrap();
    assert_eq!(mode(&root), 0o700);
}

#[test]
fn a_root_others_can_read_is_refused_with_the_fix_and_left_alone() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

    let err = open(&root).err().expect("refused");
    assert_eq!(
        err,
        HostStartError::InsecureRoot {
            path: root.clone(),
            mode: 0o755
        }
    );
    assert!(
        err.to_string()
            .contains(&format!("chmod 700 {}", root.display())),
        "{err}"
    );
    assert_eq!(mode(&root), 0o755);
    assert!(!root.join(SOCKET_FILE).exists());
}

#[test]
fn a_second_host_fails_on_the_log_lock_before_it_touches_the_socket() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    // A running host: it holds the log lock. Its socket file is one nobody listens on right now, which a bind
    // would take for stale and replace.
    let _first = JsonlEventLog::open(&root).unwrap();
    let socket = root.join(SOCKET_FILE);
    drop(UnixListener::bind(&socket).unwrap());
    let inode = fs::symlink_metadata(&socket).unwrap().ino();

    let err = open(&root).err().expect("refused");
    assert_eq!(err, HostStartError::AlreadyRunning(root.clone()));
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), inode);
}

#[test]
fn a_second_host_next_to_a_live_one_is_already_running() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let _first = open(&root).unwrap();
    assert_eq!(
        open(&root).err(),
        Some(HostStartError::AlreadyRunning(root.clone()))
    );
}

#[test]
fn the_host_log_file_is_private_and_receives_tracing_output() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let file = host::open_log_file(&root).unwrap();
    assert_eq!(mode(&root.join(LOG_FILE)), 0o600);

    tracing::subscriber::with_default(host::file_subscriber(file), || {
        tracing::error!("something went wrong");
    });
    let written = fs::read_to_string(root.join(LOG_FILE)).unwrap();
    assert!(written.contains("something went wrong"), "{written}");
}

#[test]
fn shutdown_stops_the_loop_and_releases_the_log_lock() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let mut host = open(&root).unwrap();
    host.sender().send(RuntimeEvent::Shutdown);
    host.run();
    assert!(host.is_finished());
    drop(host);

    assert!(JsonlEventLog::open(&root).is_ok());
}
