//! The archive index: `<root>/index.sqlite`, a SQLite database (WAL, busy timeout, `0600`) mapping repos, sessions,
//! snapshots and commits to blobs (T39).
//!
//! - Migrations are numbered by `PRAGMA user_version`. A database from a newer version is refused before anything
//!   writes to it (not even the switch to WAL, which changes the file header).
//! - A snapshot row is inserted only after every one of its blobs is stored, and the snapshot, its files and its commits
//!   go in one transaction, so a crash leaves at worst an orphan blob (which [`Archive::verify`] reports), never a row
//!   pointing at nothing.
//! - Every write takes SQLite's write lock up front (`BEGIN IMMEDIATE`), so concurrent writers in other processes wait
//!   on the busy timeout instead of failing.
//! - All SQL uses bound parameters. Values go in and come out as validated types; a row that doesn't parse is an error.
//! - Times are parameters, stored as microseconds since the Unix epoch.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, ToSql, TransactionBehavior, params};

use crate::blob::{BlobError, BlobId, BlobStore, Sweep};
use crate::ids::{
    AgentKind, AgentSessionId, BadValue, Branch, CommitHash, FileRole, MachineId, PatchId,
    PrNumber, RepoId, SessionId, SnapshotId, SnapshotReason,
};
use crate::private::{normalize, open_or_create_private_file};

/// The index schema version this build writes. Older indexes are migrated up to it; newer ones are refused.
pub const INDEX_VERSION: i64 = 2;

/// Migration `n` (from 1) takes an index from version `n - 1` to `n`.
const MIGRATIONS: [&str; INDEX_VERSION as usize] =
    [include_str!("schema/v1.sql"), include_str!("schema/v2.sql")];

/// How long a write waits for another process's write to finish. A generous bound: writes are short.
const BUSY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("archive index: {0}")]
    Index(#[from] rusqlite::Error),
    /// The index was written by a newer shepherd. It is left exactly as it was.
    #[error("the archive index is version {found}; this shepherd understands up to {supported}")]
    NewerIndex { found: i64, supported: i64 },
    /// SQLite kept another journal mode (named) when asked for WAL.
    #[error("the archive index would not switch to WAL (journal mode is {0})")]
    NotWal(String),
    /// The index has a WAL file but not the -shm file that goes with it, a state SQLite can't read without changing
    /// files. The index is left as it was; the user decides what to do with it.
    #[error("the archive index has a WAL file but no -shm file next to it: {0}")]
    WalWithoutShm(PathBuf),
    /// The index has a negative `user_version`, which no shepherd writes. It is left as it was.
    #[error("the archive index has version {0}, which no shepherd writes")]
    BadIndexVersion(i64),
    /// Migrating would leave a row in this table referring to a row that doesn't exist; the index is left as it was.
    #[error("migrating the archive index would break a reference in table {0}")]
    BrokenReference(String),
    #[error("session {0} is not in the archive")]
    UnknownSession(SessionId),
    #[error("snapshot {0} is not in the archive")]
    UnknownSnapshot(SnapshotId),
}

/// A session as the archive records it. [`Archive::put_session`] takes one and the lookups return them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub repo: RepoId,
    pub machine: MachineId,
    pub agent: AgentKind,
    pub agent_session_id: AgentSessionId,
    pub name: Option<String>,
    pub branch: Option<Branch>,
    pub pr: Option<PrNumber>,
    pub base_commit: Option<CommitHash>,
    pub cwd: PathBuf,
    pub created_at: Timestamp,
    pub archived_at: Option<Timestamp>,
}

/// A file to store in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFile<'a> {
    pub role: FileRole,
    /// Where the agent kept it. Unique within a snapshot.
    pub original_path: PathBuf,
    pub content: &'a [u8],
}

/// A commit the session had made when a snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCommit {
    pub commit: CommitHash,
    /// `None` for a commit `git patch-id` gives no id for (a merge, an empty commit).
    pub patch_id: Option<PatchId>,
}

/// A snapshot to add: the session's transcript files and commits at `captured_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSnapshot<'a> {
    pub session: SessionId,
    pub captured_at: Timestamp,
    pub head_commit: Option<CommitHash>,
    pub reason: SnapshotReason,
    pub files: Vec<NewFile<'a>>,
    /// Commits in the session's `base..HEAD`. One the session already recorded keeps its first snapshot.
    pub commits: Vec<SessionCommit>,
}

/// A file stored in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    pub role: FileRole,
    pub original_path: PathBuf,
    pub blob: BlobId,
    /// Its length, uncompressed.
    pub bytes: u64,
}

/// A snapshot as the archive records it, its files in the order they were given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub session: SessionId,
    pub captured_at: Timestamp,
    pub head_commit: Option<CommitHash>,
    pub reason: SnapshotReason,
    pub files: Vec<SnapshotFile>,
}

/// A file read back from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    pub file: SnapshotFile,
    pub content: Vec<u8>,
}

/// A session that made a commit, and the first of its snapshots that saw it: "the conversation as of this commit".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitHit {
    pub session: SessionId,
    pub commit: CommitHash,
    pub patch_id: Option<PatchId>,
    pub first_snapshot: SnapshotId,
}

/// A snapshot file whose blob can't be read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRow {
    pub snapshot: SnapshotId,
    pub file: SnapshotFile,
    /// Why: [`BlobError::Missing`], [`BlobError::Corrupt`], or the blob is unreadable some other way.
    pub error: BlobError,
}

/// What [`Archive::verify`] found. It only reports; nothing is deleted or changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    /// Snapshot files whose blob is missing or corrupt, by snapshot and then file order.
    pub bad_rows: Vec<BadRow>,
    /// Blobs no snapshot file refers to, sorted. A capture in progress elsewhere can show its blobs here briefly.
    pub orphans: Vec<BlobId>,
}

/// The archive under one root: its blob store and its index.
#[derive(Debug)]
pub struct Archive {
    blobs: BlobStore,
    conn: Mutex<Connection>,
}

impl Archive {
    /// Opens the archive under `root`, creating `root` (0700), its blob store and `root/index.sqlite` (0600) if
    /// missing, and migrating an older index forward. An index from a newer version is refused and not modified. An
    /// existing index that is a symlink or that group or others can access is refused and left alone.
    pub fn open(root: &Path) -> Result<Self, ArchiveError> {
        let root = normalize(root);
        let blobs = BlobStore::open(&root)?;
        // One opener at a time per archive, until this function returns. SQLite's busy timeout doesn't cover two
        // processes switching a new index to WAL at once (one gets "database is locked" straight away), and the
        // version check and migration are simpler with no other opener in between.
        let opening_path = root.join("index.lock");
        let opening = open_or_create_private_file(&opening_path)?;
        opening.lock().map_err(|e| BlobError::Io {
            path: opening_path.clone(),
            message: e.to_string(),
        })?;
        let path = root.join("index.sqlite");
        // Created here so it is 0600 from the start (SQLite gives its -wal and -shm files the same mode), and checked
        // here: a symlink or a file others can read is refused. SQLite's own NOFOLLOW can't be used, since it also
        // refuses symlinks in the parents (macOS's /var is one); and it must not create the file itself.
        open_or_create_private_file(&path)?;
        // Before anything can write: a newer (or not-SQLite) index must stay exactly as it is.
        pending_migrations(peek_version(&path)?)?;
        // Through a URI too: SQLite reads a plain filename starting with `file:` as one.
        let conn = Connection::open_with_flags(
            sqlite_uri(&path, "mode=rw"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "wal", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(ArchiveError::NotWal(mode));
        }
        let mut conn = conn;
        migrate(&mut conn)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        drop(opening);
        Ok(Self {
            blobs,
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-call can't leave a transaction open (rusqlite rolls back on drop), so the connection is usable.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many blob files this handle has written (see [`BlobStore::writes`]).
    pub fn blob_writes(&self) -> u64 {
        self.blobs.writes()
    }

    /// Records a session, or updates everything but its id and creation time if it is already recorded. Its repo is
    /// recorded too if it is new.
    pub fn put_session(&self, s: &Session) -> Result<(), ArchiveError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO repos (id) VALUES (?1) ON CONFLICT (id) DO NOTHING",
            params![s.repo],
        )?;
        tx.execute(
            "INSERT INTO sessions (id, repo_id, machine, agent, agent_session_id, name, branch, pr, base_commit, cwd,
                                   created_at, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (id) DO UPDATE SET
                 repo_id = excluded.repo_id, machine = excluded.machine, agent = excluded.agent,
                 agent_session_id = excluded.agent_session_id, name = excluded.name, branch = excluded.branch,
                 pr = excluded.pr, base_commit = excluded.base_commit, cwd = excluded.cwd,
                 archived_at = excluded.archived_at",
            params![
                s.id,
                s.repo,
                s.machine,
                s.agent,
                s.agent_session_id,
                s.name,
                s.branch,
                s.pr,
                s.base_commit,
                SqlPath(&s.cwd),
                Micros(s.created_at),
                s.archived_at.map(Micros),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stores a snapshot's files as blobs, then records the snapshot, its files and its commits in one transaction.
    /// Returns its id. A session that isn't recorded is [`ArchiveError::UnknownSession`] and nothing is stored.
    pub fn add_snapshot(&self, s: &NewSnapshot<'_>) -> Result<SnapshotId, ArchiveError> {
        if !self.session_exists(&s.session)? {
            return Err(ArchiveError::UnknownSession(s.session.clone()));
        }
        // Every blob first: the rows below must never point at a blob that isn't there.
        let mut stored = Vec::with_capacity(s.files.len());
        for f in &s.files {
            stored.push((f, self.blobs.put(f.content)?));
        }

        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The session may have gone since the check above: the foreign key refuses the row then.
        tx.execute(
            "INSERT INTO snapshots (session_id, captured_at, head_commit, reason) VALUES (?1, ?2, ?3, ?4)",
            params![s.session, Micros(s.captured_at), s.head_commit, s.reason],
        )
        .map_err(|e| match e.sqlite_error() {
            Some(f) if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                ArchiveError::UnknownSession(s.session.clone())
            }
            _ => ArchiveError::Index(e),
        })?;
        let id = SnapshotId::new(tx.last_insert_rowid());
        for (f, blob) in stored {
            let bytes = i64::try_from(f.content.len())
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            tx.execute(
                "INSERT INTO snapshot_files (snapshot_id, role, original_path, blob, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, f.role, SqlPath(&f.original_path), SqlBlobId(blob), bytes],
            )?;
        }
        for c in &s.commits {
            tx.execute(
                "INSERT INTO session_commits (session_id, commit_hash, patch_id, first_snapshot)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session_id, commit_hash) DO NOTHING",
                params![s.session, c.commit, c.patch_id, id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    fn session_exists(&self, id: &SessionId) -> Result<bool, ArchiveError> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// The sessions that made `commit` (exactly that hash), oldest session first. Empty if no session made it.
    pub fn find_by_commit(&self, commit: &CommitHash) -> Result<Vec<CommitHit>, ArchiveError> {
        self.commit_hits("c.commit_hash = ?1", commit)
    }

    /// The commits with this patch-id (the same change, perhaps rebased) and the sessions that made them.
    pub fn find_by_patch_id(&self, patch_id: &PatchId) -> Result<Vec<CommitHit>, ArchiveError> {
        self.commit_hits("c.patch_id = ?1", patch_id)
    }

    fn commit_hits(&self, filter: &str, value: &dyn ToSql) -> Result<Vec<CommitHit>, ArchiveError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT c.session_id, c.commit_hash, c.patch_id, c.first_snapshot
             FROM session_commits c JOIN sessions s ON s.id = c.session_id
             WHERE {filter}
             ORDER BY s.created_at, s.id, c.commit_hash"
        ))?;
        let rows = stmt.query_map([value], |r| {
            Ok(CommitHit {
                session: r.get(0)?,
                commit: r.get(1)?,
                patch_id: r.get(2)?,
                first_snapshot: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The sessions in `repo` that worked on `branch`: how a squash-merged commit, which no session made, is traced.
    pub fn find_by_branch(
        &self,
        repo: &RepoId,
        branch: &Branch,
    ) -> Result<Vec<Session>, ArchiveError> {
        self.sessions_where("repo_id = ?1 AND branch = ?2", params![repo, branch])
    }

    /// The sessions in `repo` whose pull request is `pr`.
    pub fn find_by_pr(&self, repo: &RepoId, pr: PrNumber) -> Result<Vec<Session>, ArchiveError> {
        self.sessions_where("repo_id = ?1 AND pr = ?2", params![repo, pr])
    }

    /// The sessions in which `agent` ran with its own session id `id`.
    pub fn find_by_agent_session(
        &self,
        agent: AgentKind,
        id: &AgentSessionId,
    ) -> Result<Vec<Session>, ArchiveError> {
        self.sessions_where("agent = ?1 AND agent_session_id = ?2", params![agent, id])
    }

    /// Every session in `repo`, oldest first.
    pub fn sessions(&self, repo: &RepoId) -> Result<Vec<Session>, ArchiveError> {
        self.sessions_where("repo_id = ?1", params![repo])
    }

    fn sessions_where(
        &self,
        filter: &str,
        values: &[&dyn ToSql],
    ) -> Result<Vec<Session>, ArchiveError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, repo_id, machine, agent, agent_session_id, name, branch, pr, base_commit, cwd, created_at,
                    archived_at
             FROM sessions WHERE {filter} ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(values, |r| {
            Ok(Session {
                id: r.get(0)?,
                repo: r.get(1)?,
                machine: r.get(2)?,
                agent: r.get(3)?,
                agent_session_id: r.get(4)?,
                name: r.get(5)?,
                branch: r.get(6)?,
                pr: r.get(7)?,
                base_commit: r.get(8)?,
                cwd: r.get::<_, OwnedPath>(9)?.0,
                created_at: r.get::<_, Micros>(10)?.0,
                archived_at: r.get::<_, Option<Micros>>(11)?.map(|m| m.0),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The session's snapshots, oldest first. Empty for a session that has none or isn't recorded.
    pub fn snapshots(&self, session: &SessionId) -> Result<Vec<Snapshot>, ArchiveError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, captured_at, head_commit, reason FROM snapshots
             WHERE session_id = ?1 ORDER BY captured_at, id",
        )?;
        let snaps = stmt
            .query_map(params![session], |r| {
                Ok(Snapshot {
                    id: r.get(0)?,
                    session: r.get(1)?,
                    captured_at: r.get::<_, Micros>(2)?.0,
                    head_commit: r.get(3)?,
                    reason: r.get(4)?,
                    files: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        snaps
            .into_iter()
            .map(|mut s| {
                s.files = snapshot_files(&conn, s.id)?;
                Ok(s)
            })
            .collect()
    }

    /// The files of a snapshot with their contents, each read from its blob and checked against its hash.
    pub fn read(&self, snapshot: SnapshotId) -> Result<Vec<ReadFile>, ArchiveError> {
        let files = {
            let conn = self.conn();
            let exists = conn
                .query_row(
                    "SELECT 1 FROM snapshots WHERE id = ?1",
                    params![snapshot],
                    |_| Ok(()),
                )
                .optional()?;
            if exists.is_none() {
                return Err(ArchiveError::UnknownSnapshot(snapshot));
            }
            snapshot_files(&conn, snapshot)?
        };
        files
            .into_iter()
            .map(|file| {
                let content = self.blobs.get(&file.blob)?;
                Ok(ReadFile { file, content })
            })
            .collect()
    }

    /// Checks every snapshot file's blob and looks for blobs nothing refers to. Reports only: it never deletes or
    /// changes a blob or a row.
    pub fn verify(&self) -> Result<VerifyReport, ArchiveError> {
        let rows: Vec<(SnapshotId, SnapshotFile)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT snapshot_id, role, original_path, blob, bytes FROM snapshot_files
                 ORDER BY snapshot_id, rowid",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, file_from_row(r, 1)?)))?
                .collect::<Result<_, _>>()?
        };
        // Each blob is read once, however many rows share it.
        let mut checked: BTreeMap<BlobId, Option<BlobError>> = BTreeMap::new();
        let mut report = VerifyReport::default();
        for (snapshot, file) in rows {
            let problem = checked
                .entry(file.blob)
                .or_insert_with(|| self.blobs.get(&file.blob).err())
                .clone();
            if let Some(error) = problem {
                report.bad_rows.push(BadRow {
                    snapshot,
                    file,
                    error,
                });
            }
        }
        let referenced: BTreeSet<BlobId> = checked.into_keys().collect();
        report.orphans = self
            .blobs
            .list()?
            .into_iter()
            .filter(|id| !referenced.contains(id))
            .collect();
        Ok(report)
    }

    /// Removes the temp files crashed blob writes left behind, under the exclusive blob writer lock (see
    /// [`BlobStore::sweep_temps`] for why a live writer's are never touched). Index rows never point at temps, so the
    /// index itself isn't locked. Maintenance: run it when convenient, not on open.
    pub fn sweep_temps(&self) -> Result<Sweep, ArchiveError> {
        Ok(self.blobs.sweep_temps()?)
    }
}

/// Reads the index's `user_version` without creating, changing or deleting any of its files. Which connection can do
/// that depends on the side files, so they are looked at (never opened) first:
/// - a -wal and its -shm: a read-only connection with `readonly_shm`, which builds the WAL's index in memory instead
///   of rewriting the -shm. (A read-write connection would checkpoint the WAL into the file and delete it on close; a
///   plain read-only one rewrites the -shm.)
/// - no -wal, or an empty one: there are no WAL frames to read, so the main file alone is read as `immutable` (no
///   locks, no side files). A stray -shm without WAL frames holds nothing.
/// - a non-empty -wal without its -shm: SQLite can't read that without making a -shm, so it is refused, untouched.
///
/// Another process closing the last connection between the look and the open can remove both side files; the open
/// then fails, possibly leaving an empty -wal, which the next open reads past as above.
fn peek_version(path: &Path) -> Result<i64, ArchiveError> {
    let side = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    // `Some(meta)` if something is at `p`. Never followed: a symlink counts as present.
    let look = |p: &Path| match fs::symlink_metadata(p) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchiveError::Blob(BlobError::Io {
            path: p.to_path_buf(),
            message: e.to_string(),
        })),
    };
    let (wal, shm) = (side("-wal"), side("-shm"));
    let has_frames = look(&wal)?.is_some_and(|m| m.file_type().is_symlink() || m.len() > 0);
    let query = match (has_frames, look(&shm)?.is_some()) {
        (true, true) => "mode=ro&readonly_shm=1",
        (false, _) => "mode=ro&immutable=1",
        (true, false) => return Err(ArchiveError::WalWithoutShm(wal)),
    };
    let conn = Connection::open_with_flags(
        sqlite_uri(path, query),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    read_version(&conn)
}

/// The bytes an SQLite URI path keeps as they are: `[A-Za-z0-9/._-]`. Everything else, `?`, `#` and `%` included, is
/// %HH-escaped, so no directory name can end or change the path.
const URI_PATH_ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'.')
    .remove(b'_')
    .remove(b'-');

/// `path` as an SQLite URI filename (https://www.sqlite.org/uri.html) with `query`.
fn sqlite_uri(path: &Path, query: &str) -> String {
    let escaped = percent_encode(path.as_os_str().as_bytes(), URI_PATH_ESCAPED);
    format!("file:{escaped}?{query}")
}

fn read_version(conn: &Connection) -> Result<i64, ArchiveError> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

/// The migrations an index at version `found` still needs, or an error if this build can't migrate it.
fn pending_migrations(found: i64) -> Result<&'static [&'static str], ArchiveError> {
    if found > INDEX_VERSION {
        return Err(ArchiveError::NewerIndex {
            found,
            supported: INDEX_VERSION,
        });
    }
    usize::try_from(found)
        .ok()
        .and_then(|from| MIGRATIONS.get(from..))
        .ok_or(ArchiveError::BadIndexVersion(found))
}

/// Brings the index up to [`INDEX_VERSION`] in one transaction, so a failed migration leaves it as it was. The version
/// is read again under SQLite's write lock, so a writer that isn't an `Archive` can't slip in between.
fn migrate(conn: &mut Connection) -> Result<(), ArchiveError> {
    if read_version(conn)? == INDEX_VERSION {
        return Ok(());
    }
    // Table rebuilds drop tables that others refer to; the check below runs before the commit instead.
    conn.pragma_update(None, "foreign_keys", false)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for sql in pending_migrations(read_version(&tx)?)? {
        tx.execute_batch(sql)?;
    }
    let violation = tx
        .query_row("PRAGMA foreign_key_check", [], |r| r.get::<_, String>(0))
        .optional()?;
    if let Some(table) = violation {
        return Err(ArchiveError::BrokenReference(table));
    }
    tx.pragma_update(None, "user_version", INDEX_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn snapshot_files(conn: &Connection, id: SnapshotId) -> Result<Vec<SnapshotFile>, ArchiveError> {
    let mut stmt = conn.prepare(
        "SELECT role, original_path, blob, bytes FROM snapshot_files WHERE snapshot_id = ?1 ORDER BY rowid",
    )?;
    let files = stmt
        .query_map(params![id], |r| file_from_row(r, 0))?
        .collect::<Result<_, _>>()?;
    Ok(files)
}

/// A [`SnapshotFile`] from `role, original_path, blob, bytes` starting at column `at`.
fn file_from_row(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<SnapshotFile> {
    Ok(SnapshotFile {
        role: r.get(at)?,
        original_path: r.get::<_, OwnedPath>(at + 1)?.0,
        blob: r.get::<_, SqlBlobIdOwned>(at + 2)?.0,
        bytes: u64::try_from(r.get::<_, i64>(at + 3)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                at + 3,
                rusqlite::types::Type::Integer,
                e.into(),
            )
        })?,
    })
}

// ---- conversions between the typed values and SQL ----

fn parse_text<T: FromStr<Err = BadValue>>(v: ValueRef<'_>) -> FromSqlResult<T> {
    v.as_str()?
        .parse()
        .map_err(|e: BadValue| FromSqlError::Other(e.into()))
}

macro_rules! sql_text {
    ($($t:ty),+ $(,)?) => {$(
        impl ToSql for $t {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                let s: &str = self.as_str();
                Ok(ToSqlOutput::from(s))
            }
        }

        impl FromSql for $t {
            fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
                parse_text(v)
            }
        }
    )+};
}

sql_text!(
    CommitHash,
    PatchId,
    SessionId,
    AgentSessionId,
    MachineId,
    Branch,
    RepoId,
    AgentKind,
    SnapshotReason,
    FileRole,
);

impl ToSql for PrNumber {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(i64::from(self.get())))
    }
}

impl FromSql for PrNumber {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let n = v.as_i64()?;
        u32::try_from(n)
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .map(PrNumber::new)
            .ok_or(FromSqlError::OutOfRange(n))
    }
}

impl ToSql for SnapshotId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.get()))
    }
}

impl FromSql for SnapshotId {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(SnapshotId::new(v.as_i64()?))
    }
}

/// A blob id as the index stores it: its 64-hex-digit name.
struct SqlBlobId(BlobId);

impl ToSql for SqlBlobId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

struct SqlBlobIdOwned(BlobId);

impl FromSql for SqlBlobIdOwned {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        v.as_str()?
            .parse()
            .map(SqlBlobIdOwned)
            .map_err(|e: crate::blob::BadBlobId| FromSqlError::Other(e.into()))
    }
}

/// A path as the index stores it: its bytes (a Unix path need not be UTF-8).
struct SqlPath<'a>(&'a Path);

impl ToSql for SqlPath<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.as_os_str().as_bytes()))
    }
}

struct OwnedPath(PathBuf);

impl FromSql for OwnedPath {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(OwnedPath(PathBuf::from(OsString::from_vec(
            v.as_blob()?.to_vec(),
        ))))
    }
}

/// A time as the index stores it: microseconds since the Unix epoch. Finer precision is dropped.
#[derive(Clone, Copy)]
struct Micros(Timestamp);

impl ToSql for Micros {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.as_microsecond()))
    }
}

impl FromSql for Micros {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let n = v.as_i64()?;
        Timestamp::from_microsecond(n)
            .map(Micros)
            .map_err(|_| FromSqlError::OutOfRange(n))
    }
}
