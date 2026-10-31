//! The archive index: SQLite rows mapping repos, sessions, snapshots and commits to blobs (T39). Tempdirs only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use jiff::Timestamp;
use shepherd_archive::{
    AgentKind, AgentSessionId, Archive, ArchiveError, BlobError, BlobId, BlobStore, CommitHash,
    CommitHit, FileRole, INDEX_VERSION, MachineId, NewFile, NewSnapshot, PatchId, PrNumber, RepoId,
    Session, SessionCommit, SessionId, SnapshotId, SnapshotReason, Sweep,
};
use tempfile::TempDir;

fn tempdir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every file under `dir`, recursively, with its bytes.
fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            out.extend(files_under(&path));
        } else {
            let bytes = fs::read(&path).unwrap();
            out.push((path, bytes));
        }
    }
    out.sort();
    out
}

fn hash(c: char) -> CommitHash {
    c.to_string().repeat(40).parse().unwrap()
}

fn patch(c: char) -> PatchId {
    c.to_string().repeat(40).parse().unwrap()
}

fn repo(c: char) -> RepoId {
    RepoId::from_root_commit(hash(c))
}

fn sid(s: &str) -> SessionId {
    s.parse().unwrap()
}

fn pr(n: u32) -> PrNumber {
    PrNumber::new(NonZeroU32::new(n).unwrap())
}

fn at(micros: i64) -> Timestamp {
    Timestamp::from_microsecond(micros).unwrap()
}

fn session(id: &str, repo_id: RepoId) -> Session {
    Session {
        id: sid(id),
        repo: repo_id,
        machine: "laptop".parse::<MachineId>().unwrap(),
        agent: AgentKind::Claude,
        agent_session_id: format!("agent-{id}").parse::<AgentSessionId>().unwrap(),
        name: Some(format!("session {id}")),
        branch: None,
        pr: None,
        base_commit: Some(hash('b')),
        cwd: PathBuf::from(format!("/work/{id}")),
        created_at: at(1_767_225_600_000_000),
        archived_at: None,
    }
}

fn file<'a>(role: FileRole, path: &str, content: &'a [u8]) -> NewFile<'a> {
    NewFile {
        role,
        original_path: PathBuf::from(path),
        content,
    }
}

fn snapshot<'a>(
    session: &str,
    files: Vec<NewFile<'a>>,
    commits: Vec<SessionCommit>,
) -> NewSnapshot<'a> {
    NewSnapshot {
        session: sid(session),
        captured_at: at(1_767_229_200_123_456),
        head_commit: commits.last().map(|c| c.commit.clone()),
        reason: SnapshotReason::TurnEnd,
        files,
        commits,
    }
}

fn commit(c: char, p: Option<char>) -> SessionCommit {
    SessionCommit {
        commit: hash(c),
        patch_id: p.map(patch),
    }
}

/// A raw connection to the index file, for what the public API deliberately can't do (check pragmas, plant rows).
fn raw(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join("index.sqlite")).unwrap()
}

fn user_version(root: &Path) -> i64 {
    raw(root)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

// ---- open, files and migrations ----

#[test]
fn open_creates_a_private_index_in_wal_mode_at_the_current_version() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let archive = Archive::open(&root).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();

    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("index.sqlite")), 0o600);
    for side in ["index.sqlite-wal", "index.sqlite-shm"] {
        assert_eq!(mode(&root.join(side)), 0o600, "{side} is private too");
    }
    let mode_name: String = raw(&root)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode_name, "wal");
    assert_eq!(user_version(&root), INDEX_VERSION);
}

#[test]
fn reopening_keeps_what_was_written() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    Archive::open(&root)
        .unwrap()
        .put_session(&session("s1", repo('a')))
        .unwrap();
    let again = Archive::open(&root).unwrap();
    assert_eq!(
        again.sessions(&repo('a')).unwrap(),
        vec![session("s1", repo('a'))]
    );
}

#[test]
fn open_refuses_a_symlinked_index_and_writes_nothing_through_it() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    BlobStore::open(&root).unwrap();
    let target = dir.path().join("elsewhere.sqlite");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&target)
        .unwrap();
    symlink(&target, root.join("index.sqlite")).unwrap();

    assert!(matches!(
        Archive::open(&root),
        Err(ArchiveError::Blob(BlobError::Symlink(_)))
    ));
    assert_eq!(
        fs::read(&target).unwrap(),
        b"",
        "nothing written through the link"
    );
}

#[test]
fn open_refuses_an_index_group_or_others_can_read_and_leaves_it_alone() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    Archive::open(&root).unwrap();
    let index = root.join("index.sqlite");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        Archive::open(&root),
        Err(ArchiveError::Blob(BlobError::Insecure { mode: 0o644, .. }))
    ));
    assert_eq!(mode(&index), 0o644, "an existing index is never chmodded");
}

/// Builds `root/index.sqlite` (0600, in a private root) from SQL, as another program would have left it.
fn plant_index(root: &Path, sql: &str) {
    BlobStore::open(root).unwrap();
    let path = root.join("index.sqlite");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(sql).unwrap();
}

#[test]
fn a_v1_index_migrates_forward_and_keeps_its_rows() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    plant_index(&root, include_str!("fixtures/index-v1.sql"));
    let blobs = BlobStore::open(&root).unwrap();
    let main = blobs.put(b"fixture transcript\n").unwrap();
    let sub = blobs.put(b"fixture subagent\n").unwrap();

    let archive = Archive::open(&root).unwrap();
    assert_eq!(user_version(&root), INDEX_VERSION);

    let v1_repo: RepoId = "1111111111111111111111111111111111111111".parse().unwrap();
    let sessions = archive.sessions(&v1_repo).unwrap();
    assert_eq!(
        sessions,
        vec![Session {
            id: sid("s-v1"),
            repo: v1_repo.clone(),
            machine: "laptop".parse().unwrap(),
            agent: AgentKind::Claude,
            agent_session_id: "0b6f3c1e-7d2a-4c55-9f00-000000000001".parse().unwrap(),
            name: Some("fix the parser".to_owned()),
            branch: Some("fix-parser".parse().unwrap()),
            pr: Some(pr(7)),
            base_commit: Some(hash('2')),
            cwd: PathBuf::from("/work/project/.shepherd/fix-parser"),
            created_at: at(1_767_225_600_000_000),
            archived_at: None,
        }]
    );
    let snaps = archive.snapshots(&sid("s-v1")).unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].captured_at, at(1_767_229_200_123_456));
    assert_eq!(snaps[0].reason, SnapshotReason::TurnEnd);
    assert_eq!(snaps[0].head_commit, Some(hash('3')));
    let read = archive.read(snaps[0].id).unwrap();
    let got: Vec<_> = read
        .iter()
        .map(|f| (f.file.role, f.file.blob, f.content.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (FileRole::Main, main, b"fixture transcript\n".to_vec()),
            (FileRole::Subagent, sub, b"fixture subagent\n".to_vec()),
        ]
    );
    assert_eq!(
        archive.find_by_patch_id(&patch('4')).unwrap(),
        vec![CommitHit {
            session: sid("s-v1"),
            commit: hash('3'),
            patch_id: Some(patch('4')),
            first_snapshot: snaps[0].id,
        }]
    );
    assert_eq!(
        archive.find_by_pr(&v1_repo, pr(7)).unwrap().len(),
        1,
        "the migrated rows are reachable by every lookup"
    );

    // The migrated schema enforces what v1 didn't: a reason outside the four is refused.
    let err = raw(&root).execute(
        "INSERT INTO snapshots (session_id, captured_at, reason) VALUES ('s-v1', 0, 'bogus')",
        [],
    );
    assert!(err.is_err(), "v2 checks the reason");
}

#[test]
fn an_index_from_a_newer_version_is_refused_and_not_modified() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let newer = INDEX_VERSION + 1;
    plant_index(
        &root,
        &format!(
            "CREATE TABLE future(x INTEGER); INSERT INTO future VALUES (1); PRAGMA user_version = {newer};"
        ),
    );
    let before = index_files(&root);
    assert_eq!(before.len(), 1);

    for _ in 0..2 {
        match Archive::open(&root) {
            Err(ArchiveError::NewerIndex { found, supported }) => {
                assert_eq!((found, supported), (newer, INDEX_VERSION));
            }
            other => panic!("expected NewerIndex, got {other:?}"),
        }
    }
    assert_eq!(
        index_files(&root),
        before,
        "the index is byte-for-byte untouched and no SQLite side file was made"
    );
}

#[test]
fn an_index_with_a_negative_version_is_refused_and_not_migrated() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    plant_index(
        &root,
        "CREATE TABLE odd(x INTEGER); PRAGMA user_version = -1;",
    );
    let before = fs::read(root.join("index.sqlite")).unwrap();
    assert!(matches!(
        Archive::open(&root),
        Err(ArchiveError::BadIndexVersion(-1))
    ));
    assert_eq!(fs::read(root.join("index.sqlite")).unwrap(), before);
}

/// The index and every SQLite side file (-wal, -shm, -journal) next to it, with their bytes. The archive's own lock
/// files are not included: they may appear.
fn index_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    files_under(root)
        .into_iter()
        .filter(|(p, _)| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("index.sqlite"))
        })
        .collect()
}

#[test]
fn a_newer_index_with_unmerged_wal_frames_is_refused_and_not_modified() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    plant_newer_wal_index(
        dir.path(),
        &root,
        &["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"],
    );
    let wal = fs::metadata(root.join("index.sqlite-wal")).unwrap().len();
    assert!(wal > 0, "the WAL holds frames the main file doesn't");

    assert_opens_leave_index_files_alone(
        &root,
        |r| matches!(r, Err(ArchiveError::NewerIndex { found, .. }) if *found == INDEX_VERSION + 1),
    );
}

#[test]
fn a_newer_wal_mode_index_that_was_closed_cleanly_is_refused_and_not_modified() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let newer = INDEX_VERSION + 1;
    // Closing the only connection checkpoints the WAL and deletes -wal and -shm: only index.sqlite is left.
    plant_index(
        &root,
        &format!(
            "PRAGMA journal_mode = WAL; CREATE TABLE future(x INTEGER); PRAGMA user_version = {newer};"
        ),
    );
    assert_eq!(index_files(&root).len(), 1);

    assert_opens_leave_index_files_alone(
        &root,
        |r| matches!(r, Err(ArchiveError::NewerIndex { found, .. }) if *found == newer),
    );
}

#[test]
fn a_wal_without_its_shm_is_refused_and_not_touched() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    plant_newer_wal_index(dir.path(), &root, &["index.sqlite", "index.sqlite-wal"]);

    assert_opens_leave_index_files_alone(
        &root,
        |r| matches!(r, Err(ArchiveError::WalWithoutShm(p)) if p.ends_with("index.sqlite-wal")),
    );
}

#[test]
fn an_empty_wal_without_its_shm_holds_no_frames_and_is_read_past() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let newer = INDEX_VERSION + 1;
    plant_index(
        &root,
        &format!(
            "PRAGMA journal_mode = WAL; CREATE TABLE future(x INTEGER); PRAGMA user_version = {newer};"
        ),
    );
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("index.sqlite-wal"))
        .unwrap();

    assert_opens_leave_index_files_alone(
        &root,
        |r| matches!(r, Err(ArchiveError::NewerIndex { found, .. }) if *found == newer),
    );
}

/// Builds a newer-version WAL-mode index in `scratch/src` with a connection that stays open while `names` are copied
/// into `root`, as a crashed newer shepherd would leave them: the newer version lives only in WAL frames that were
/// never checkpointed into the main file.
fn plant_newer_wal_index(scratch: &Path, root: &Path, names: &[&str]) {
    BlobStore::open(root).unwrap();
    let src = scratch.join("src");
    fs::create_dir(&src).unwrap();
    let writer = rusqlite::Connection::open(src.join("index.sqlite")).unwrap();
    let newer = INDEX_VERSION + 1;
    writer
        .execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE future(x INTEGER); INSERT INTO future VALUES (1); PRAGMA user_version = {newer};"
        ))
        .unwrap();
    for name in names {
        fs::copy(src.join(name), root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(writer);
    assert_eq!(index_files(root).len(), names.len());
}

/// Opens `root` twice, checks each result with `expected`, and asserts that no `index.sqlite*` file was added,
/// removed or changed by a single byte.
fn assert_opens_leave_index_files_alone(
    root: &Path,
    expected: impl Fn(&Result<Archive, ArchiveError>) -> bool,
) {
    let before = index_files(root);
    for _ in 0..2 {
        let result = Archive::open(root);
        assert!(expected(&result), "unexpected open result: {result:?}");
    }
    let after = index_files(root);
    assert_eq!(
        after.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        before.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        "no index file added or removed"
    );
    for ((path, was), (_, is)) in before.iter().zip(&after) {
        assert!(was == is, "{} was rewritten", path.display());
    }
}

#[test]
fn a_file_that_is_not_sqlite_is_refused_and_left_byte_identical() {
    for body in [
        b"this is not a SQLite database, it is a text file\n".repeat(50),
        b"hi".to_vec(),
    ] {
        let dir = tempdir();
        let root = dir.path().join("archive");
        BlobStore::open(&root).unwrap();
        let index = root.join("index.sqlite");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&index)
            .unwrap()
            .write_all(&body)
            .unwrap();
        let before = index_files(&root);

        match Archive::open(&root) {
            Err(ArchiveError::Index(e)) => assert_eq!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::NotADatabase),
                "{e}"
            ),
            other => panic!("expected NotADatabase, got {other:?}"),
        }
        assert_eq!(index_files(&root), before, "{} bytes", body.len());
    }
}

#[test]
fn an_archive_under_a_path_with_uri_characters_opens_and_reopens() {
    let dir = tempdir();
    let parent = dir.path().join("a ?b#c%41d&e=f");
    fs::create_dir(&parent).unwrap();
    let root = parent.join("archive");
    let first = Archive::open(&root).unwrap();
    first.put_session(&session("s1", repo('a'))).unwrap();
    // Opened again while the first handle is live (-wal and -shm exist), and after it closed (they don't).
    assert_eq!(
        Archive::open(&root)
            .unwrap()
            .sessions(&repo('a'))
            .unwrap()
            .len(),
        1
    );
    drop(first);
    assert_eq!(
        Archive::open(&root)
            .unwrap()
            .sessions(&repo('a'))
            .unwrap()
            .len(),
        1
    );
    assert!(
        !dir.path().join("a ").exists(),
        "nothing was created at a truncated path"
    );
}

/// The v1 fixture plus `extra` SQL, as a v1 index.
fn plant_v1(root: &Path, extra: &str) {
    plant_index(
        root,
        &format!("{}\n{extra}", include_str!("fixtures/index-v1.sql")),
    );
}

/// The planted v1 index, read raw: its schema (every table and index, as SQL) and its rows (every table's contents
/// as text). A half-applied migration changes one or the other.
fn v1_state(root: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let conn = raw(root);
    let schema = conn
        .prepare("SELECT type || ' ' || name || ': ' || coalesce(sql, '') FROM sqlite_master ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let rows = [
        "repos",
        "sessions",
        "snapshots",
        "snapshot_files",
        "session_commits",
    ]
    .into_iter()
    .map(|t| {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {t} ORDER BY rowid"))
            .unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |r| {
            (0..n)
                .map(|i| {
                    r.get::<_, rusqlite::types::Value>(i)
                        .map(|v| format!("{v:?}"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|cols| cols.join("|"))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    })
    .collect();
    (schema, rows)
}

#[test]
fn a_v1_row_that_breaks_a_v2_constraint_fails_the_migration_and_leaves_v1_intact() {
    for bad in [
        "INSERT INTO snapshots VALUES (2, 's-v1', 1, NULL, 'bogus');",
        "INSERT INTO sessions VALUES ('s-null-cwd', '1111111111111111111111111111111111111111', 'laptop', 'codex',
                                      'x', NULL, NULL, NULL, NULL, NULL, 1, NULL);",
    ] {
        let dir = tempdir();
        let root = dir.path().join("archive");
        plant_v1(&root, bad);
        let state = v1_state(&root);

        assert!(
            matches!(Archive::open(&root), Err(ArchiveError::Index(_))),
            "{bad}"
        );
        assert_eq!(user_version(&root), 1, "still v1 after {bad}");
        assert_eq!(v1_state(&root), state, "v1 schema and rows intact after {bad}");
    }
}

#[test]
fn a_v1_row_with_a_dangling_reference_fails_the_migration_and_leaves_v1_intact() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    plant_v1(
        &root,
        "INSERT INTO snapshots VALUES (2, 'nobody', 1, NULL, 'manual');",
    );
    let state = v1_state(&root);

    match Archive::open(&root) {
        Err(ArchiveError::BrokenReference(table)) => assert_eq!(table, "snapshots"),
        other => panic!("expected BrokenReference, got {other:?}"),
    }
    assert_eq!(user_version(&root), 1);
    assert_eq!(v1_state(&root), state);
}

// ---- sessions and snapshots ----

#[test]
fn put_session_updates_an_existing_session() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    let mut s = session("s1", repo('a'));
    archive.put_session(&s).unwrap();
    s.branch = Some("feature".parse().unwrap());
    s.pr = Some(pr(12));
    s.archived_at = Some(at(1_767_300_000_000_001));
    archive.put_session(&s).unwrap();
    assert_eq!(archive.sessions(&repo('a')).unwrap(), vec![s]);
}

#[test]
fn sessions_lists_only_that_repos_sessions_oldest_first() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    let mut later = session("later", repo('a'));
    later.created_at = at(2_000_000_000_000_000);
    let earlier = session("earlier", repo('a'));
    archive.put_session(&later).unwrap();
    archive.put_session(&earlier).unwrap();
    archive.put_session(&session("other", repo('c'))).unwrap();

    assert_eq!(archive.sessions(&repo('a')).unwrap(), vec![earlier, later]);
    assert_eq!(archive.sessions(&repo('d')).unwrap(), vec![]);
}

#[test]
fn a_snapshot_round_trips_through_snapshots_and_read() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let id = archive
        .add_snapshot(&snapshot(
            "s1",
            vec![
                file(FileRole::Main, "/t/main.jsonl", b"main transcript"),
                file(FileRole::Subagent, "/t/main/subagents/x.jsonl", b"sub"),
            ],
            vec![commit('c', Some('7'))],
        ))
        .unwrap();

    let snaps = archive.snapshots(&sid("s1")).unwrap();
    assert_eq!(snaps.len(), 1);
    let s = &snaps[0];
    assert_eq!(s.id, id);
    assert_eq!(s.session, sid("s1"));
    assert_eq!(
        s.captured_at,
        at(1_767_229_200_123_456),
        "the time passed in, to the microsecond"
    );
    assert_eq!(s.head_commit, Some(hash('c')));
    assert_eq!(s.reason, SnapshotReason::TurnEnd);
    let files: Vec<_> = s
        .files
        .iter()
        .map(|f| (f.role, f.original_path.clone(), f.blob, f.bytes))
        .collect();
    assert_eq!(
        files,
        vec![
            (
                FileRole::Main,
                PathBuf::from("/t/main.jsonl"),
                BlobId::of(b"main transcript"),
                15
            ),
            (
                FileRole::Subagent,
                PathBuf::from("/t/main/subagents/x.jsonl"),
                BlobId::of(b"sub"),
                3
            ),
        ]
    );
    let contents: Vec<_> = archive
        .read(id)
        .unwrap()
        .into_iter()
        .map(|f| (f.file.original_path, f.content))
        .collect();
    assert_eq!(
        contents,
        vec![
            (PathBuf::from("/t/main.jsonl"), b"main transcript".to_vec()),
            (PathBuf::from("/t/main/subagents/x.jsonl"), b"sub".to_vec()),
        ]
    );
}

#[test]
fn every_reason_and_role_round_trips() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let reasons = [
        SnapshotReason::TurnEnd,
        SnapshotReason::Close,
        SnapshotReason::BeforeRemove,
        SnapshotReason::Manual,
    ];
    for (i, reason) in reasons.into_iter().enumerate() {
        let mut s = snapshot(
            "s1",
            vec![
                file(FileRole::Main, "/m", b"m"),
                file(FileRole::Subagent, "/s", b"s"),
                file(FileRole::Rollout, "/r", b"r"),
            ],
            vec![],
        );
        s.reason = reason;
        s.captured_at = at(i as i64);
        archive.add_snapshot(&s).unwrap();
    }
    let snaps = archive.snapshots(&sid("s1")).unwrap();
    assert_eq!(snaps.iter().map(|s| s.reason).collect::<Vec<_>>(), reasons);
    assert_eq!(
        snaps[0].files.iter().map(|f| f.role).collect::<Vec<_>>(),
        vec![FileRole::Main, FileRole::Subagent, FileRole::Rollout]
    );
}

#[test]
fn a_snapshot_of_unchanged_files_adds_a_row_but_no_blob() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let files = || vec![file(FileRole::Main, "/t/main.jsonl", b"same")];
    archive
        .add_snapshot(&snapshot("s1", files(), vec![]))
        .unwrap();
    let written = archive.blob_writes();
    archive
        .add_snapshot(&snapshot("s1", files(), vec![]))
        .unwrap();
    assert_eq!(
        archive.blob_writes(),
        written,
        "no new blob for unchanged content"
    );
    assert_eq!(archive.snapshots(&sid("s1")).unwrap().len(), 2);
}

#[test]
fn a_snapshot_for_an_unknown_session_is_refused_and_stores_nothing() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let archive = Archive::open(&root).unwrap();
    let err = archive
        .add_snapshot(&snapshot(
            "nobody",
            vec![file(FileRole::Main, "/m", b"m")],
            vec![],
        ))
        .unwrap_err();
    assert!(matches!(err, ArchiveError::UnknownSession(s) if s == sid("nobody")));
    assert_eq!(BlobStore::open(&root).unwrap().list().unwrap(), vec![]);
}

#[test]
fn snapshots_of_an_unknown_session_is_empty_and_read_of_an_unknown_snapshot_is_an_error() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    assert_eq!(archive.snapshots(&sid("nobody")).unwrap(), vec![]);
    assert!(matches!(
        archive.read(SnapshotId::new(42)),
        Err(ArchiveError::UnknownSnapshot(id)) if id == SnapshotId::new(42)
    ));
}

// ---- lookups ----

#[test]
fn find_by_commit_finds_the_session_and_the_first_snapshot_that_saw_it() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let first = archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"v1")],
            vec![commit('c', Some('7'))],
        ))
        .unwrap();
    // A later capture sees the same commit again plus a new one.
    let second = archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"v2")],
            vec![commit('c', Some('7')), commit('d', None)],
        ))
        .unwrap();

    assert_eq!(
        archive.find_by_commit(&hash('c')).unwrap(),
        vec![CommitHit {
            session: sid("s1"),
            commit: hash('c'),
            patch_id: Some(patch('7')),
            first_snapshot: first,
        }]
    );
    assert_eq!(
        archive.find_by_commit(&hash('d')).unwrap(),
        vec![CommitHit {
            session: sid("s1"),
            commit: hash('d'),
            patch_id: None,
            first_snapshot: second,
        }]
    );
}

#[test]
fn find_by_commit_of_an_unknown_commit_is_empty() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"m")],
            vec![commit('c', Some('7'))],
        ))
        .unwrap();
    assert_eq!(archive.find_by_commit(&hash('e')).unwrap(), vec![]);
    // A sha256 name that starts with a known sha1 name is a different commit.
    let longer: CommitHash = format!("{}{}", hash('c'), "c".repeat(24)).parse().unwrap();
    assert_eq!(archive.find_by_commit(&longer).unwrap(), vec![]);
}

#[test]
fn find_by_patch_id_finds_a_commit_after_a_rebase() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let snap = archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"m")],
            vec![commit('c', Some('7'))],
        ))
        .unwrap();
    // After a rebase the commit is 'e', unknown by hash; the same change has the same patch-id.
    assert_eq!(archive.find_by_commit(&hash('e')).unwrap(), vec![]);
    assert_eq!(
        archive.find_by_patch_id(&patch('7')).unwrap(),
        vec![CommitHit {
            session: sid("s1"),
            commit: hash('c'),
            patch_id: Some(patch('7')),
            first_snapshot: snap,
        }]
    );
    assert_eq!(archive.find_by_patch_id(&patch('f')).unwrap(), vec![]);
}

#[test]
fn a_squash_merge_commit_is_found_by_branch_and_by_pr_within_its_repo() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    let mut s = session("s1", repo('a'));
    s.branch = Some("fix-parser".parse().unwrap());
    s.pr = Some(pr(12));
    archive.put_session(&s).unwrap();
    archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"m")],
            vec![commit('c', Some('7')), commit('d', Some('8'))],
        ))
        .unwrap();
    // The same branch name and PR number in another repo is another session.
    let mut elsewhere = session("s2", repo('e'));
    elsewhere.branch = Some("fix-parser".parse().unwrap());
    elsewhere.pr = Some(pr(12));
    archive.put_session(&elsewhere).unwrap();

    // GitHub's squash commit 'f' (patch-id '9') was made by no session.
    assert_eq!(archive.find_by_commit(&hash('f')).unwrap(), vec![]);
    assert_eq!(archive.find_by_patch_id(&patch('9')).unwrap(), vec![]);
    assert_eq!(
        archive
            .find_by_branch(&repo('a'), &"fix-parser".parse().unwrap())
            .unwrap(),
        vec![s.clone()]
    );
    assert_eq!(archive.find_by_pr(&repo('a'), pr(12)).unwrap(), vec![s]);
    assert_eq!(
        archive
            .find_by_branch(&repo('a'), &"other".parse().unwrap())
            .unwrap(),
        vec![]
    );
    assert_eq!(archive.find_by_pr(&repo('a'), pr(13)).unwrap(), vec![]);
}

#[test]
fn find_by_agent_session_finds_the_session_by_the_agents_own_id() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    let claude = session("s1", repo('a'));
    let mut codex = session("s2", repo('a'));
    codex.agent = AgentKind::Codex;
    codex.agent_session_id = claude.agent_session_id.clone();
    archive.put_session(&claude).unwrap();
    archive.put_session(&codex).unwrap();

    assert_eq!(
        archive
            .find_by_agent_session(AgentKind::Claude, &claude.agent_session_id)
            .unwrap(),
        vec![claude.clone()],
        "the same id under another agent is another session"
    );
    assert_eq!(
        archive
            .find_by_agent_session(AgentKind::Codex, &claude.agent_session_id)
            .unwrap(),
        vec![codex]
    );
    assert_eq!(
        archive
            .find_by_agent_session(AgentKind::Claude, &"unknown".parse().unwrap())
            .unwrap(),
        vec![]
    );
}

// ---- verify ----

#[test]
fn verify_of_a_healthy_archive_reports_nothing() {
    let dir = tempdir();
    let archive = Archive::open(&dir.path().join("archive")).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    archive
        .add_snapshot(&snapshot(
            "s1",
            vec![file(FileRole::Main, "/m", b"m")],
            vec![],
        ))
        .unwrap();
    let report = archive.verify().unwrap();
    assert!(report.bad_rows.is_empty(), "{report:?}");
    assert!(report.orphans.is_empty(), "{report:?}");
}

#[test]
fn verify_reports_missing_and_corrupt_blobs_and_orphans_and_deletes_nothing() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let archive = Archive::open(&root).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    let snap = archive
        .add_snapshot(&snapshot(
            "s1",
            vec![
                file(FileRole::Main, "/missing", b"will be deleted"),
                file(FileRole::Subagent, "/corrupt", b"will be corrupted"),
                file(FileRole::Rollout, "/fine", b"fine"),
            ],
            vec![],
        ))
        .unwrap();
    let blobs = BlobStore::open(&root).unwrap();
    let orphan = blobs.put(b"nobody refers to me").unwrap();
    let path_of = |content: &[u8]| {
        let hex = BlobId::of(content).to_string();
        root.join("blobs")
            .join(&hex[..2])
            .join(format!("{hex}.zst"))
    };
    fs::remove_file(path_of(b"will be deleted")).unwrap();
    fs::write(
        path_of(b"will be corrupted"),
        zstd::encode_all(&b"other bytes"[..], 3).unwrap(),
    )
    .unwrap();
    let before = files_under(&root.join("blobs"));
    let rows_before = archive.snapshots(&sid("s1")).unwrap();

    let report = archive.verify().unwrap();

    let bad: Vec<_> = report
        .bad_rows
        .iter()
        .map(|r| (r.snapshot, r.file.original_path.clone(), r.error.clone()))
        .collect();
    assert_eq!(bad.len(), 2, "{report:?}");
    assert!(matches!(
        &bad[0],
        (s, p, BlobError::Missing { id, .. }) if *s == snap && p == Path::new("/missing") && *id == BlobId::of(b"will be deleted")
    ));
    assert!(matches!(
        &bad[1],
        (s, p, BlobError::Corrupt { id, .. }) if *s == snap && p == Path::new("/corrupt") && *id == BlobId::of(b"will be corrupted")
    ));
    assert_eq!(report.orphans, vec![orphan]);

    assert_eq!(
        files_under(&root.join("blobs")),
        before,
        "verify deletes and changes no blob"
    );
    assert_eq!(
        archive.snapshots(&sid("s1")).unwrap(),
        rows_before,
        "nor any row"
    );
}

// ---- processes: crash, concurrency, temp sweep ----

/// The parent passes the child its archive root, and what to do there, through these variables.
const CHILD_ROOT: &str = "T39_CHILD_ROOT";
const CHILD_JOB: &str = "T39_CHILD_JOB";
/// What a paused child prints once its temp file is written and it holds the writer lock.
const PAUSED: &str = shepherd_archive::PAUSE_MARKER;
const WRITES_PER_CHILD: usize = 25;

fn crash_content() -> Vec<u8> {
    let mut content = shepherd_archive::CRASH_BEFORE_RENAME.to_vec();
    content.extend_from_slice(b"the second file");
    content
}

fn pause_content() -> Vec<u8> {
    let mut content = shepherd_archive::PAUSE_BEFORE_RENAME.to_vec();
    content.extend_from_slice(b"written by a live writer");
    content
}

/// Runs only as a child process started by `child`; does nothing in a normal test run.
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
    let job = job.to_str().unwrap();
    match job {
        "crash-mid-snapshot" => {
            let archive = Archive::open(root).unwrap();
            let crash = crash_content();
            archive
                .add_snapshot(&snapshot(
                    "s1",
                    vec![
                        file(FileRole::Main, "/first", b"the first file"),
                        file(FileRole::Subagent, "/second", &crash),
                    ],
                    vec![commit('c', Some('7'))],
                ))
                .unwrap();
            unreachable!("put must exit the process before the rename");
        }
        "pause-mid-put" => {
            BlobStore::open(root)
                .unwrap()
                .put(&pause_content())
                .unwrap();
        }
        "snapshot-paused-mid-put" => {
            let archive = Archive::open(root).unwrap();
            let paused = pause_content();
            // The session exists when this starts; the parent deletes it while the blob write is paused.
            let result = archive.add_snapshot(&snapshot(
                "s1",
                vec![file(FileRole::Main, "/m", &paused)],
                vec![],
            ));
            assert!(
                matches!(&result, Err(ArchiveError::UnknownSession(s)) if *s == sid("s1")),
                "the foreign key refused the snapshot: {result:?}"
            );
        }
        "relative-file-root" => {
            // Run from a scratch cwd with the relative root `file:arch`, which SQLite would read as a URI if it were
            // ever handed the plain path.
            let archive = Archive::open(root).unwrap();
            archive.put_session(&session("s1", repo('a'))).unwrap();
            drop(archive);
            assert!(root.join("index.sqlite").is_file());
            let again = Archive::open(root).unwrap();
            assert_eq!(again.sessions(&repo('a')).unwrap().len(), 1);
        }
        opener if opener.starts_with("opener-") => {
            Archive::open(root)
                .unwrap()
                .put_session(&session(opener, repo('a')))
                .unwrap();
        }
        "umask-0" => {
            // SAFETY: umask only sets this process's file mode mask; this child runs a single test.
            unsafe { libc::umask(0) };
            let archive = Archive::open(root).unwrap();
            archive.put_session(&session("s1", repo('a'))).unwrap();
            // Checked here, while the connection is open: SQLite removes -wal and -shm when the last one closes.
            for name in [
                "index.sqlite",
                "index.sqlite-wal",
                "index.sqlite-shm",
                "index.lock",
                "blobs.lock",
            ] {
                assert_eq!(mode(&root.join(name)), 0o600, "{name} under umask 0");
            }
        }
        writer if writer.starts_with("writer-") => {
            let archive = Archive::open(root).unwrap();
            for i in 0..WRITES_PER_CHILD {
                let id = format!("{writer}-{i}");
                archive.put_session(&session(&id, repo('a'))).unwrap();
                let own = format!("{id} transcript");
                archive
                    .add_snapshot(&snapshot(
                        &id,
                        vec![
                            file(FileRole::Main, "/own", own.as_bytes()),
                            // Both writers store this one: dedupe under contention.
                            file(
                                FileRole::Subagent,
                                "/shared",
                                format!("shared {i}").as_bytes(),
                            ),
                        ],
                        vec![],
                    ))
                    .unwrap();
            }
        }
        other => panic!("unknown child job {other:?}"),
    }
}

/// Starts `child_process` in a new process with `job` on `root`.
fn child(job: &str, root: &Path) -> Child {
    child_command(job, root).spawn().unwrap()
}

fn child_command(job: &str, root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "child_process",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_ROOT, root)
        .env(CHILD_JOB, job)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    command
}

#[test]
fn a_relative_root_starting_with_file_colon_is_a_path_not_a_uri() {
    let dir = tempdir();
    let status = child_command("relative-file-root", Path::new("file:arch"))
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child opened and reopened file:arch");
    assert!(dir.path().join("file:arch").join("index.sqlite").is_file());
    assert!(
        !dir.path().join("arch").exists(),
        "nothing went to the path a URI parse would give"
    );
}

#[test]
fn open_waits_while_another_process_holds_the_index_lock() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let held = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("index.lock"))
        .unwrap();
    held.lock().unwrap();

    let mut opener = child("opener-0", &root);
    drop(opener.stdin.take());
    // The child creates blobs.lock just before it asks for index.lock. Spin (no sleep) until it has.
    while !root.join("blobs.lock").exists() {
        assert!(
            opener.try_wait().unwrap().is_none(),
            "the opener exited before reaching the lock"
        );
        std::thread::yield_now();
    }
    // A trap only an opener that didn't wait could fall into: an index that isn't SQLite. Creating it must succeed
    // (the opener hasn't made its own index yet), and the opener must never see it.
    let trap = root.join("index.sqlite");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&trap)
        .expect("the opener created no index while the lock was held")
        .write_all(b"not a database")
        .unwrap();
    assert!(
        opener.try_wait().unwrap().is_none(),
        "the opener is waiting"
    );
    fs::remove_file(&trap).unwrap();
    drop(held);

    assert!(
        opener.wait_with_output().unwrap().status.success(),
        "the opener went on once the lock was released"
    );
    assert_eq!(
        Archive::open(&root)
            .unwrap()
            .sessions(&repo('a'))
            .unwrap()
            .len(),
        1
    );
}

fn run_child(job: &str, root: &Path) -> Option<i32> {
    let mut c = child(job, root);
    drop(c.stdin.take());
    c.wait_with_output().unwrap().status.code()
}

/// Waits until `c` reports it is paused mid-put, holding the blob writer lock.
fn wait_paused(c: &mut Child) {
    let mut out = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    while !line.trim_end().ends_with(PAUSED) {
        line.clear();
        assert_ne!(
            out.read_line(&mut line).unwrap(),
            0,
            "the child paused before exiting"
        );
    }
    // Back to the child, so its later output still has a reader.
    c.stdout = Some(out.into_inner());
}

/// Lets a child paused by [`wait_paused`] go on, and returns whether it then succeeded.
fn release(mut c: Child) -> bool {
    c.stdin.take().unwrap().write_all(b"go").unwrap();
    c.wait_with_output().unwrap().status.success()
}

#[test]
fn modes_are_exact_under_umask_0_including_sqlites_side_files() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    assert_eq!(
        run_child("umask-0", &root),
        Some(0),
        "the child checked every mode"
    );
}

#[test]
fn foreign_keys_are_enforced_after_open() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    Archive::open(&root)
        .unwrap()
        .put_session(&session("s1", repo('a')))
        .unwrap();

    let mut c = child("snapshot-paused-mid-put", &root);
    wait_paused(&mut c);
    // Behind the archive's back (a raw connection doesn't enforce foreign keys), the session goes away.
    raw(&root)
        .execute("DELETE FROM sessions WHERE id = 's1'", [])
        .unwrap();
    assert!(
        release(c),
        "the child's snapshot was refused as UnknownSession"
    );

    let snapshots: i64 = raw(&root)
        .query_row("SELECT count(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        snapshots, 0,
        "no snapshot row points at the missing session"
    );
}

#[test]
fn racing_opens_of_a_new_archive_all_succeed() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let mut openers: Vec<_> = (0..6)
        .map(|i| child(&format!("opener-{i}"), &root))
        .collect();
    for c in &mut openers {
        drop(c.stdin.take());
    }
    for (i, c) in openers.iter_mut().enumerate() {
        assert!(c.wait().unwrap().success(), "opener {i} succeeded");
    }
    assert_eq!(
        Archive::open(&root)
            .unwrap()
            .sessions(&repo('a'))
            .unwrap()
            .len(),
        6
    );
}

#[test]
fn a_crash_while_storing_a_snapshots_blobs_leaves_no_row_and_verify_reports_the_orphan() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    Archive::open(&root)
        .unwrap()
        .put_session(&session("s1", repo('a')))
        .unwrap();

    assert_eq!(
        run_child("crash-mid-snapshot", &root),
        Some(shepherd_archive::CRASH_EXIT_CODE),
        "the child crashed storing the second blob"
    );

    let archive = Archive::open(&root).unwrap();
    assert_eq!(
        archive.snapshots(&sid("s1")).unwrap(),
        vec![],
        "no snapshot row"
    );
    assert_eq!(
        archive.find_by_commit(&hash('c')).unwrap(),
        vec![],
        "no commit row"
    );
    let report = archive.verify().unwrap();
    assert!(
        report.bad_rows.is_empty(),
        "no row points at a missing blob: {report:?}"
    );
    assert_eq!(report.orphans, vec![BlobId::of(b"the first file")]);
}

#[test]
fn two_processes_writing_at_once_both_succeed_and_nothing_is_lost() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    // Neither process finds an index: both race to create and migrate it too.
    let mut a = child("writer-a", &root);
    let mut b = child("writer-b", &root);
    drop(a.stdin.take());
    drop(b.stdin.take());
    assert!(a.wait().unwrap().success(), "writer a succeeded");
    assert!(b.wait().unwrap().success(), "writer b succeeded");

    let archive = Archive::open(&root).unwrap();
    let sessions = archive.sessions(&repo('a')).unwrap();
    assert_eq!(sessions.len(), 2 * WRITES_PER_CHILD);
    for s in &sessions {
        let snaps = archive.snapshots(&s.id).unwrap();
        assert_eq!(snaps.len(), 1, "{} has its snapshot", s.id);
        let read = archive.read(snaps[0].id).unwrap();
        assert_eq!(read[0].content, format!("{} transcript", s.id).into_bytes());
    }
    let report = archive.verify().unwrap();
    assert!(
        report.bad_rows.is_empty() && report.orphans.is_empty(),
        "{report:?}"
    );
}

/// The temp files `put` leaves in shard dirs (names start with `.` and end with `.tmp`).
fn temps(root: &Path) -> Vec<PathBuf> {
    files_under(&root.join("blobs"))
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
        .collect()
}

#[test]
fn sweep_removes_temps_a_crashed_put_left_and_keeps_every_blob() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let archive = Archive::open(&root).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    assert_eq!(
        run_child("crash-mid-snapshot", &root),
        Some(shepherd_archive::CRASH_EXIT_CODE)
    );
    let stale = temps(&root);
    assert_eq!(stale.len(), 1, "the crash left a temp file");
    // A file in a shard that only looks temp-ish is not ours to remove.
    let shard = stale[0].parent().unwrap().to_path_buf();
    fs::write(shard.join("notes.tmp"), b"not a put temp").unwrap();
    // Nor is a validly named temp in another blob's shard.
    let other_shard = root
        .join("blobs")
        .join(if shard.ends_with("00") { "01" } else { "00" });
    fs::create_dir_all(&other_shard).unwrap();
    fs::set_permissions(&other_shard, fs::Permissions::from_mode(0o700)).unwrap();
    let misplaced = other_shard.join(stale[0].file_name().unwrap());
    fs::write(&misplaced, b"misplaced").unwrap();
    // Nor a directory named exactly like a temp in the right shard.
    let dir_id = BlobId::of(b"a directory").to_string();
    let dir_shard = root.join("blobs").join(&dir_id[..2]);
    fs::create_dir_all(&dir_shard).unwrap();
    fs::set_permissions(&dir_shard, fs::Permissions::from_mode(0o700)).unwrap();
    let temp_named_dir = dir_shard.join(format!(".{dir_id}.1.0.tmp"));
    fs::create_dir(&temp_named_dir).unwrap();
    let blobs_before: Vec<_> = files_under(&root.join("blobs"))
        .into_iter()
        .filter(|(p, _)| p.extension().is_some_and(|e| e == "zst"))
        .collect();

    assert_eq!(
        archive.sweep_temps().unwrap(),
        Sweep::Swept { removed: stale }
    );

    let mut left = vec![shard.join("notes.tmp"), misplaced];
    left.sort();
    assert_eq!(temps(&root), left);
    assert!(temp_named_dir.is_dir(), "the directory is left alone");
    let blobs_after: Vec<_> = files_under(&root.join("blobs"))
        .into_iter()
        .filter(|(p, _)| p.extension().is_some_and(|e| e == "zst"))
        .collect();
    assert_eq!(blobs_after, blobs_before, "no blob touched");
    assert_eq!(
        archive.sweep_temps().unwrap(),
        Sweep::Swept { removed: vec![] }
    );
}

#[test]
fn sweep_never_touches_the_temp_of_a_live_writer() {
    let dir = tempdir();
    let root = dir.path().join("archive");
    let archive = Archive::open(&root).unwrap();
    archive.put_session(&session("s1", repo('a'))).unwrap();
    assert_eq!(
        run_child("crash-mid-snapshot", &root),
        Some(shepherd_archive::CRASH_EXIT_CODE)
    );
    let stale = temps(&root);

    // A live writer that has written its temp file and is about to rename it.
    let mut writer = child("pause-mid-put", &root);
    wait_paused(&mut writer);
    assert_eq!(temps(&root).len(), 2, "the live writer's temp is there");

    assert_eq!(
        archive.sweep_temps().unwrap(),
        Sweep::Busy,
        "a writer is mid-put"
    );
    assert_eq!(
        temps(&root).len(),
        2,
        "nothing removed while a writer is live"
    );

    assert!(release(writer), "the writer renamed its temp into place");
    let id = BlobId::of(&pause_content());
    assert_eq!(
        BlobStore::open(&root).unwrap().get(&id).unwrap(),
        pause_content()
    );

    assert_eq!(
        archive.sweep_temps().unwrap(),
        Sweep::Swept { removed: stale }
    );
    assert_eq!(temps(&root), Vec::<PathBuf>::new());
}
