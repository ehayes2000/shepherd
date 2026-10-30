//! A startup error reaches `host.log` too. Alone in its own test binary: tracing caches each callsite's interest
//! process-wide, so another test hitting `open`'s error callsite with no subscriber, at the same moment, could make
//! this test's thread-local subscriber miss it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use shepherd::host::{self, CONFIG_FILE, LOG_FILE};

#[test]
fn a_startup_error_is_also_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let mut config = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(CONFIG_FILE))
        .unwrap();
    std::io::Write::write_all(&mut config, b"not = [toml").unwrap();
    let file = host::open_log_file(&root).unwrap();

    let result = tracing::subscriber::with_default(host::file_subscriber(file), || {
        host::open(&root, PathBuf::from("/"), Some("/bin/sh".into()))
    });
    assert!(result.is_err());
    let logged = fs::read_to_string(root.join(LOG_FILE)).unwrap();
    assert!(logged.contains("host failed to start"), "{logged}");
    assert!(logged.contains(CONFIG_FILE), "{logged}");
}
