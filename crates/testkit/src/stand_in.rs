//! Stand-in executables for tests, none of them a file written by the test.
//!
//! macOS has Gatekeeper and XProtect assess an executable on its first exec, and does that one file at a time: under
//! the load of parallel test runs, a freshly written `#!/bin/sh` script can wait longer than any liveness bound before
//! it starts (F45, F46). So every stand-in is a symlink to one checked-in script, `fixtures/stand-in`, which is
//! assessed once and then runs a body that is plain data: `<path>.sh`, sourced with `$0` and the arguments unchanged.
//! (`shepherd-adapters` has its own copy for its unit tests.)

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The checked-in script every stand-in links to.
pub const SHARED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/stand-in");

/// Installs at `path` an executable that runs `body` as a `sh` script, `$0` being `path`.
///
/// # Panics
/// If `path` or `<path>.sh` can't be written.
#[allow(clippy::unwrap_used)]
pub fn install(path: &Path, body: &str) {
    std::fs::write(body_path(path), body).unwrap();
    std::os::unix::fs::symlink(SHARED, path).unwrap();
}

/// Where [`install`] puts the body of the stand-in at `path`.
pub fn body_path(path: &Path) -> PathBuf {
    let mut body = OsString::from(path);
    body.push(".sh");
    body.into()
}
