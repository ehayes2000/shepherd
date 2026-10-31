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
    assert!(
        store.inflated() <= 1025,
        "inflation stops one byte past the cap, not at the end of the bomb: {} bytes",
        store.inflated()
    );
}

/// Writes `body` as the blob file for `content`, 0600, the way a corrupted or tampered archive would hold it.
fn plant_blob_file(root: &Path, content: &[u8], body: &[u8]) -> PathBuf {
    let path = blob_path(root, content);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[test]
fn put_heals_an_existing_blob_whose_content_does_not_match() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let id = store.put(b"original").unwrap();
    plant_blob_file(
        &root,
        b"original",
        &zstd::encode_all(&b"tampered"[..], 3).unwrap(),
    );

    assert_eq!(store.put(b"original").unwrap(), id);

    assert_eq!(store.writes(), 2, "the bad blob was replaced");
    assert_eq!(store.get(&id).unwrap(), b"original");
    assert_eq!(mode(&blob_path(&root, b"original")), 0o600);
}

#[test]
fn put_heals_an_existing_truncated_blob() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let content: Vec<u8> = (0..50_000u32).flat_map(|n| n.to_le_bytes()).collect();
    let id = store.put(&content).unwrap();
    let full = fs::read(blob_path(&root, &content)).unwrap();
    plant_blob_file(&root, &content, &full[..full.len() / 2]);

    assert_eq!(store.put(&content).unwrap(), id);

    assert_eq!(store.writes(), 2);
    assert_eq!(store.get(&id).unwrap(), content);
}

#[test]
fn put_over_an_existing_blob_group_or_others_can_read_is_refused_and_leaves_it_alone() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    store.put(b"real").unwrap();
    let path = blob_path(&root, b"real");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(matches!(
        store.put(b"real"),
        Err(BlobError::Insecure { mode: 0o644, .. })
    ));
    assert_eq!(store.writes(), 1);
    assert_eq!(mode(&path), 0o644, "never chmodded");
}

#[test]
fn put_over_a_symlink_at_the_blob_path_is_refused_and_writes_nothing_outside() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    store.put(b"real").unwrap();
    let path = blob_path(&root, b"real");
    let outside = dir.path().join("outside");
    fs::write(&outside, b"not yours").unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();

    assert!(matches!(store.put(b"real"), Err(BlobError::Symlink(p)) if p == path));
    assert_eq!(store.writes(), 1);
    assert_eq!(fs::read(&outside).unwrap(), b"not yours");
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn put_heals_an_existing_blob_that_inflates_past_the_cap() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap().with_max_len(1024);
    let id = store.put(b"real").unwrap();
    plant_blob_file(
        &root,
        b"real",
        &zstd::encode_all(&[0u8; 64 * 1024][..], 3).unwrap(),
    );
    assert!(matches!(store.get(&id), Err(BlobError::TooLarge { .. })));

    assert_eq!(store.put(b"real").unwrap(), id);

    assert_eq!(store.writes(), 2, "the oversized blob was replaced");
    assert_eq!(store.get(&id).unwrap(), b"real");
}

/// A 16-byte zstd frame whose header claims `size` bytes of content: magic, a descriptor for a single-segment frame
/// with an 8-byte content size, the size, and one empty raw last block.
fn frame_claiming(size: u64) -> Vec<u8> {
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0xE0];
    frame.extend_from_slice(&size.to_le_bytes());
    frame.extend_from_slice(&[0x01, 0x00, 0x00]);
    frame
}

#[test]
fn a_frame_header_claiming_a_huge_size_reserves_no_more_than_the_cap() {
    assert_eq!(
        zstd::zstd_safe::get_frame_content_size(&frame_claiming(1 << 60)).ok(),
        Some(Some(1 << 60)),
        "the planted frame really claims 2^60 bytes"
    );
    let dir = tempdir();
    let root = dir.path().join("archive");
    // In a child, so a regression aborts the child (allocation failure) and fails this test, not the test binary.
    assert_eq!(run_child("huge-header", &root), Some(0));
}

#[test]
fn put_over_a_directory_at_the_blob_path_is_refused() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let store = BlobStore::open(&root).unwrap();
    let path = blob_path(&root, b"real");
    fs::create_dir_all(&path).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(store.put(b"real"), Err(BlobError::WrongType(p)) if p == path));
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
    // A correctly named blob in the wrong shard, and a directory named like a blob in its own shard.
    let misplaced = BlobId::of(b"misplaced").to_string();
    let wrong_shard = if &misplaced[..2] == "00" { "01" } else { "00" };
    fs::create_dir_all(root.join("blobs").join(wrong_shard)).unwrap();
    fs::write(
        root.join("blobs")
            .join(wrong_shard)
            .join(format!("{misplaced}.zst")),
        b"x",
    )
    .unwrap();
    fs::create_dir_all(blob_path(&root, b"a directory")).unwrap();

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

/// The parent passes the child its archive root, and what to do there, through these variables.
const CHILD_ROOT: &str = "T38_CHILD_ROOT";
const CHILD_JOB: &str = "T38_CHILD_JOB";

/// Runs only as a child process started by `run_child`; does nothing in a normal test run.
#[test]
fn child_process() {
    #[allow(
        clippy::disallowed_methods,
        reason = "test plumbing: the parent passes the root and job to its child"
    )]
    let (Some(root), Some(job)) = (std::env::var_os(CHILD_ROOT), std::env::var_os(CHILD_JOB))
    else {
        return;
    };
    let root = Path::new(&root);
    match job.to_str() {
        Some("crash") => {
            let store = BlobStore::open(root).unwrap();
            store.put(&crash_content()).unwrap();
            unreachable!("put must exit the process before the rename");
        }
        Some("umask") => {
            // SAFETY: umask only sets this process's file mode mask; this child runs a single test.
            unsafe { libc::umask(0o277) };
            let store = BlobStore::open(root).unwrap();
            store.put(b"under a strict umask").unwrap();
        }
        Some("huge-header") => {
            let store = BlobStore::open(root).unwrap().with_max_len(1024);
            let id = store.put(b"claims a lot").unwrap();
            plant_blob_file(root, b"claims a lot", &frame_claiming(1 << 60));
            // Without the clamp this reserves 2^60 bytes and the allocator aborts the process.
            assert!(store.get(&id).is_err());
        }
        other => panic!("unknown child job {other:?}"),
    }
}

/// Runs `child_process` in a new process with `job` on `root` and returns its exit code.
fn run_child(job: &str, root: &Path) -> Option<i32> {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_process", "--test-threads=1"])
        .env(CHILD_ROOT, root)
        .env(CHILD_JOB, job)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .code()
}

fn crash_content() -> Vec<u8> {
    let mut content = shepherd_archive::CRASH_BEFORE_RENAME.to_vec();
    content.extend_from_slice(&[7u8; 100_000]);
    content
}

#[test]
fn modes_are_exact_whatever_the_umask() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    assert_eq!(
        run_child("umask", &root),
        Some(0),
        "the child stored a blob under umask 277"
    );
    let path = blob_path(&root, b"under a strict umask");
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("blobs")), 0o700);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn a_crash_between_temp_write_and_rename_leaves_no_blob_under_its_id() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    BlobStore::open(&root).unwrap();

    assert_eq!(
        run_child("crash", &root),
        Some(shepherd_archive::CRASH_EXIT_CODE),
        "the child crashed where we injected it"
    );

    let content = crash_content();
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
