//! A process's build identity (T36a): the digest of its executable, and what is installed at the host's startup path
//! now, recomputed only when that file changes.
#![allow(clippy::unwrap_used)]

use std::fs;

use shepherd::build::{InstalledExe, build_id};
use shepherd_core::wire::{DigestHasher, ExeDigest};

fn sha256(bytes: &[u8]) -> ExeDigest {
    let mut hasher = DigestHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

#[test]
fn a_build_is_the_crate_version_and_the_digest_of_the_executable() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    fs::write(&exe, b"build one").unwrap();
    let build = build_id(&exe).unwrap();
    assert_eq!(build.digest, sha256(b"build one"));
    assert_eq!(build.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn the_installed_digest_is_recomputed_only_when_the_file_at_the_path_changes() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    fs::write(&exe, b"build one").unwrap();
    let mut installed = InstalledExe::new(&exe);
    assert_eq!(installed.digest(), Some(sha256(b"build one")));
    assert_eq!(installed.digest(), Some(sha256(b"build one")));
    assert_eq!(installed.digests_computed(), 1);

    // Installed as `cargo install` does: a new file renamed over the path.
    let next = dir.path().join("shepherd.new");
    fs::write(&next, b"build two, longer").unwrap();
    fs::rename(&next, &exe).unwrap();
    assert_eq!(installed.digest(), Some(sha256(b"build two, longer")));
    assert_eq!(installed.digest(), Some(sha256(b"build two, longer")));
    assert_eq!(installed.digests_computed(), 2);
}

#[test]
fn a_file_renamed_over_the_path_with_the_same_size_and_mtime_is_told_apart_by_its_inode() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    fs::write(&exe, b"build one").unwrap();
    let mtime = fs::metadata(&exe).unwrap().modified().unwrap();
    let mut installed = InstalledExe::new(&exe);
    assert_eq!(installed.digest(), Some(sha256(b"build one")));

    let next = dir.path().join("shepherd.new");
    fs::write(&next, b"build two").unwrap();
    fs::File::options()
        .write(true)
        .open(&next)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::rename(&next, &exe).unwrap();
    let meta = fs::metadata(&exe).unwrap();
    assert_eq!((meta.len(), meta.modified().unwrap()), (9, mtime));
    assert_eq!(installed.digest(), Some(sha256(b"build two")));
    assert_eq!(installed.digests_computed(), 2);
}

#[test]
fn nothing_installed_at_the_path_has_no_digest() {
    let dir = tempfile::tempdir().unwrap();
    let mut installed = InstalledExe::new(dir.path().join("gone"));
    assert_eq!(installed.digest(), None);
}
