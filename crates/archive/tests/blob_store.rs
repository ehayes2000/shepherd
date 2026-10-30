//! The blob store: content-addressed, zstd-compressed, private (T38). Tempdirs only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};
use shepherd_archive::{BlobError, BlobId, BlobStore};
use tempfile::TempDir;

fn tempdir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where the spec says a blob lives, computed independently of the store.
fn blob_path(root: &Path, content: &[u8]) -> PathBuf {
    let hex = sha256_hex(content);
    root.join("blobs")
        .join(&hex[..2])
        .join(format!("{hex}.zst"))
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn open_creates_the_root_0700_and_blobs_inside_it() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    BlobStore::open(&root).unwrap();
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("blobs")), 0o700);
}

#[test]
fn open_reuses_an_existing_private_root() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let id = BlobStore::open(&root).unwrap().put(b"kept").unwrap();
    let again = BlobStore::open(&root).unwrap();
    assert_eq!(again.get(&id).unwrap(), b"kept");
}

#[test]
fn open_refuses_a_symlinked_root() {
    let dir = tempdir();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("archive");
    symlink(&real, &link).unwrap();
    assert!(matches!(BlobStore::open(&link), Err(BlobError::Symlink(p)) if p == link));
    // A trailing slash would make the OS resolve the link: it must not get past the check.
    let slash = PathBuf::from(format!("{}/", link.display()));
    assert!(matches!(
        BlobStore::open(&slash),
        Err(BlobError::Symlink(_))
    ));
    assert!(
        !real.join("blobs").exists(),
        "nothing is created through the link"
    );
}

#[test]
fn open_refuses_an_existing_root_group_or_others_can_access_and_leaves_it_alone() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(matches!(
        BlobStore::open(&root),
        Err(BlobError::Insecure { mode: 0o750, .. })
    ));
    assert_eq!(mode(&root), 0o750, "an existing root is never chmodded");
    assert!(!root.join("blobs").exists());
}

#[test]
fn open_refuses_a_root_that_is_a_file() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    fs::write(&root, b"").unwrap();
    assert!(matches!(
        BlobStore::open(&root),
        Err(BlobError::WrongType(_))
    ));
}

#[test]
fn open_refuses_a_symlinked_blobs_dir() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    BlobStore::open(&root).unwrap();
    let elsewhere = tempdir();
    fs::remove_dir(root.join("blobs")).unwrap();
    symlink(elsewhere.path(), root.join("blobs")).unwrap();
    assert!(matches!(BlobStore::open(&root), Err(BlobError::Symlink(_))));
}

#[test]
fn put_writes_a_zstd_file_named_by_the_sha256_of_the_content() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let content = b"{\"type\":\"user\",\"message\":\"hello\"}\n".repeat(100);

    let id = store.put(&content).unwrap();

    assert_eq!(id.to_string(), sha256_hex(&content));
    let path = blob_path(&root, &content);
    let on_disk = fs::read(&path).unwrap();
    assert!(on_disk.len() < content.len(), "the body is compressed");
    assert_eq!(zstd::decode_all(&on_disk[..]).unwrap(), content);
    assert_eq!(files_under(&root.join("blobs")), vec![path]);
}

#[test]
fn blob_files_are_0600_and_shard_dirs_0700() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    store.put(b"secret").unwrap();
    let path = blob_path(&root, b"secret");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[test]
fn identical_content_is_stored_once() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();

    let first = store.put(b"same").unwrap();
    assert_eq!(store.writes(), 1);
    let second = store.put(b"same").unwrap();

    assert_eq!(first, second);
    assert_eq!(store.writes(), 1, "the second put writes nothing");
    assert_eq!(files_under(&root.join("blobs")).len(), 1);

    store.put(b"different").unwrap();
    assert_eq!(store.writes(), 2);
}

#[test]
fn get_returns_what_was_put() {
    let dir = tempdir();
    let store = BlobStore::open(&dir.path().join("archive")).unwrap();
    let empty = store.put(b"").unwrap();
    let big: Vec<u8> = (0..200_000u32).flat_map(|n| n.to_le_bytes()).collect();
    let id = store.put(&big).unwrap();
    assert_eq!(store.get(&id).unwrap(), big);
    assert_eq!(store.get(&empty).unwrap(), b"");
}

#[test]
fn get_of_an_unknown_id_is_missing() {
    let dir = tempdir();
    let store = BlobStore::open(&dir.path().join("archive")).unwrap();
    let id = BlobId::of(b"never stored");
    assert!(matches!(store.get(&id), Err(BlobError::Missing { id: missing, .. }) if missing == id));
}

#[test]
fn a_blob_whose_content_does_not_match_its_name_is_corrupt_and_named() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let id = store.put(b"original").unwrap();
    // Valid zstd, wrong content: the hash check must catch it.
    let path = blob_path(&root, b"original");
    fs::write(&path, zstd::encode_all(&b"tampered"[..], 3).unwrap()).unwrap();

    let err = store.get(&id).unwrap_err();
    assert!(
        matches!(&err, BlobError::Corrupt { id: bad, .. } if *bad == id),
        "{err:?}"
    );
    assert!(err.to_string().contains(&id.to_string()), "{err}");
}

#[test]
fn a_truncated_blob_is_corrupt_and_named() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let content: Vec<u8> = (0..50_000u32).flat_map(|n| n.to_le_bytes()).collect();
    let id = store.put(&content).unwrap();
    let path = blob_path(&root, &content);
    let full = fs::read(&path).unwrap();
    fs::write(&path, &full[..full.len() / 2]).unwrap();

    let err = store.get(&id).unwrap_err();
    assert!(
        matches!(&err, BlobError::Corrupt { id: bad, .. } if *bad == id),
        "{err:?}"
    );
    assert!(err.to_string().contains(&id.to_string()), "{err}");
}

#[test]
fn a_blob_that_is_not_zstd_is_corrupt() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let id = store.put(b"original").unwrap();
    fs::write(blob_path(&root, b"original"), b"not zstd at all").unwrap();
    assert!(matches!(store.get(&id), Err(BlobError::Corrupt { .. })));
}

#[test]
fn a_blob_that_inflates_past_the_cap_is_an_error_not_an_allocation() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap().with_max_len(1024);
    // A hostile file: tiny on disk, far bigger than the cap once inflated, under a name that matches its content.
    let bomb = vec![0u8; 64 * 1024 * 1024];
    let path = blob_path(&root, &bomb);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, zstd::encode_all(&bomb[..], 19).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let err = store.get(&BlobId::of(&bomb)).unwrap_err();
    assert!(
        matches!(&err, BlobError::TooLarge { max: 1024, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains(&sha256_hex(&bomb)), "{err}");
}

#[test]
fn put_refuses_content_past_the_cap_so_every_stored_blob_is_readable() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap().with_max_len(4);
    assert!(store.put(b"1234").is_ok());
    assert!(matches!(
        store.put(b"12345"),
        Err(BlobError::TooLarge { max: 4, .. })
    ));
    assert_eq!(store.writes(), 1);
}

#[test]
fn get_refuses_a_blob_that_is_a_symlink() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let id = store.put(b"real").unwrap();
    let path = blob_path(&root, b"real");
    let outside = dir.path().join("outside.zst");
    fs::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(matches!(store.get(&id), Err(BlobError::Symlink(p)) if p == path));
}

#[test]
fn get_refuses_a_blob_group_or_others_can_read() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let id = store.put(b"real").unwrap();
    fs::set_permissions(blob_path(&root, b"real"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(store.get(&id), Err(BlobError::Insecure { .. })));
}

#[test]
fn list_enumerates_stored_blob_ids_and_ignores_anything_else() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    assert_eq!(store.list().unwrap(), vec![]);

    let a = store.put(b"a").unwrap();
    let b = store.put(b"b").unwrap();
    let c = store.put(b"c").unwrap();
    // Leftovers a crash or a stranger could leave behind are not blobs.
    let shard = blob_path(&root, b"a").parent().unwrap().to_path_buf();
    fs::write(shard.join(".leftover.tmp"), b"x").unwrap();
    fs::write(shard.join("README"), b"x").unwrap();
    fs::write(root.join("blobs").join("stray"), b"x").unwrap();
    fs::create_dir(root.join("blobs").join("zz")).unwrap();

    let mut expected = vec![a, b, c];
    expected.sort();
    assert_eq!(store.list().unwrap(), expected);
}

#[test]
fn a_blob_id_round_trips_through_its_hex_form() {
    let id = BlobId::of(b"x");
    assert_eq!(id.to_string().parse::<BlobId>().unwrap(), id);
    assert!("not hex".parse::<BlobId>().is_err());
    assert!(sha256_hex(b"x")[..62].parse::<BlobId>().is_err());
    assert!(
        sha256_hex(b"x").to_uppercase().parse::<BlobId>().is_err(),
        "one spelling per id"
    );
}

/// The parent passes the child its archive root through this variable.
const CHILD_ROOT: &str = "T38_CRASH_CHILD_ROOT";

/// Runs only as the child of `a_crash_between_temp_write_and_rename_leaves_no_blob_under_its_id`.
#[test]
fn crash_child() {
    #[allow(
        clippy::disallowed_methods,
        reason = "test plumbing: the parent passes the root to its child"
    )]
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let store = BlobStore::open(Path::new(&root)).unwrap();
    let mut content = shepherd_archive::CRASH_BEFORE_RENAME.to_vec();
    content.extend_from_slice(&[7u8; 100_000]);
    store.put(&content).unwrap();
    unreachable!("put must exit the process before the rename");
}

#[test]
fn a_crash_between_temp_write_and_rename_leaves_no_blob_under_its_id() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    BlobStore::open(&root).unwrap();

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--test-threads=1"])
        .env(CHILD_ROOT, &root)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(shepherd_archive::CRASH_EXIT_CODE),
        "the child crashed where we injected it"
    );

    let mut content = shepherd_archive::CRASH_BEFORE_RENAME.to_vec();
    content.extend_from_slice(&[7u8; 100_000]);
    let path = blob_path(&root, &content);
    assert!(!path.exists(), "no partial blob under its id");
    let leftovers = files_under(&root.join("blobs"));
    assert_eq!(
        leftovers.len(),
        1,
        "the temp file was written: {leftovers:?}"
    );

    let store = BlobStore::open(&root).unwrap();
    assert_eq!(store.list().unwrap(), vec![], "a temp file is never listed");
    let id = BlobId::of(&content);
    assert!(matches!(store.get(&id), Err(BlobError::Missing { .. })));
}
