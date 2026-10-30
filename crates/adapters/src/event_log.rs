//! ACP++ logs as JSONL files, one envelope per line (see "Storage" in `docs/acpp.md`):
//!
//! ```text
//! <root>/lock                      # held while a JsonlEventLog is open
//! <root>/host.jsonl
//! <root>/sessions/<session-id>.jsonl
//! ```
//!
//! Durability (decision 0009): each append is one `write` of the whole line, newline included, with no fsync.
//! An entry is durable against a crash of this process once `append` returns, but not against power loss or a
//! kernel panic.
//!
//! [`JsonlEventLog::open`] scans every log once and repairs what a crash or power loss can leave at its end,
//! reporting each repair through [`JsonlEventLog::recovered`] so the runtime can warn:
//! - bytes after the last newline (a torn write) are truncated;
//! - a trailing region of only NUL bytes and newlines after the last entry (unwritten blocks after power loss)
//!   is truncated.
//!
//! A complete line that doesn't parse is never truncated, wherever it is, since it may be an entry from a newer
//! shepherd. It is [`LogError::Corrupt`], as are seq gaps and entries for another log.
//!
//! A store has a single writer: `open` takes an exclusive lock on `<root>/lock` for the adapter's lifetime, and
//! a second `open` gets [`LogError::Locked`]. The adapter keeps an index of where each entry starts, so reads
//! seek to the first wanted entry and appends check the file is still the length it left it at.
//!
//! The logs hold prompts and tool output, so the store is private (decision 0010), whatever the umask: the root
//! and `sessions/` are created `0700`, and the lock and log files `0600`, exactly (see [`crate::private_fs`]).
//! Anything that already exists and is accessible to group or others, the root included, is
//! [`LogError::InsecurePermissions`] and is never chmodded: it is the user's data, and they fix it. A symlink
//! where the root, `sessions/`, the lock or a log goes is refused with [`LogError::Io`] and never followed (not
//! skipped, so the user notices), whether it is found on open, read or append.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use shepherd_core::acpp::{Envelope, FrameDraft, Seq, Stamp};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LogError, LogId};

use crate::private_fs::{self, PrivateFsError};

const EXT: &str = "jsonl";

/// Leftovers that [`JsonlEventLog::open`] cut off the end of a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TornTail {
    pub log: LogId,
    /// How many bytes were removed.
    pub bytes: u64,
}

/// What an append needs from a log file. A seam so tests can make writes and rollbacks fail.
trait AppendFile {
    fn len(&self) -> io::Result<u64>;
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
}

impl AppendFile for File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }
}

type OpenAppendFile = fn(&Path) -> Result<Box<dyn AppendFile>, LogError>;

fn open_append(path: &Path) -> Result<File, LogError> {
    open_private(path, OpenOptions::new().append(true))
}

fn open_append_file(path: &Path) -> Result<Box<dyn AppendFile>, LogError> {
    Ok(Box::new(open_append(path)?))
}

fn private_err(e: PrivateFsError) -> LogError {
    match e {
        PrivateFsError::Insecure { path, mode } => LogError::InsecurePermissions { path, mode },
        other => LogError::Io(other.to_string()),
    }
}

fn ensure_private_dir(dir: &Path) -> Result<(), LogError> {
    private_fs::ensure_private_dir(dir).map_err(private_err)
}

fn open_private(path: &Path, opts: &OpenOptions) -> Result<File, LogError> {
    private_fs::create_or_open_private_file(path, opts).map_err(private_err)
}

fn open_existing(path: &Path, opts: &OpenOptions) -> Result<File, LogError> {
    private_fs::open_existing_private_file(path, opts).map_err(private_err)
}

fn refuse_symlink(path: &Path, file_type: fs::FileType) -> Result<(), LogError> {
    private_fs::refuse_symlink(path, file_type).map_err(private_err)
}

/// Where a log's entries are in its file.
#[derive(Debug, Default)]
struct Index {
    /// The byte offset of each entry: `starts[i]` is where seq `i + 1` begins.
    starts: Vec<u64>,
    /// The file length after the last entry.
    end: u64,
    /// A failed append left bytes after `end` that couldn't be removed.
    poisoned: bool,
}

pub struct JsonlEventLog {
    root: PathBuf,
    logs: BTreeMap<LogId, Index>,
    recovered: Vec<TornTail>,
    open_append: OpenAppendFile,
    /// Holds the store's exclusive lock until dropped.
    lock: File,
}

impl Drop for JsonlEventLog {
    /// Unlocks explicitly rather than by closing the file: a child forked in the meantime (a PTY spawn) shares
    /// the lock until it execs, so closing our descriptor alone wouldn't release it.
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl JsonlEventLog {
    /// Opens (creating if needed) the logs under `root`, repairing their ends. Fails with
    /// [`LogError::Locked`] if another `JsonlEventLog` has `root` open, and with
    /// [`LogError::InsecurePermissions`] if the root or anything in it is accessible to group or others.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LogError> {
        Self::open_with(root, open_append_file)
    }

    fn open_with(root: impl AsRef<Path>, open_append: OpenAppendFile) -> Result<Self, LogError> {
        let root = root.as_ref().to_path_buf();
        ensure_private_dir(&root)?;
        ensure_private_dir(&root.join("sessions"))?;
        let lock = open_private(&root.join("lock"), OpenOptions::new().write(true))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(LogError::Locked),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
        let mut log = Self {
            root,
            logs: BTreeMap::new(),
            recovered: Vec::new(),
            open_append,
            lock,
        };
        for id in log.stored_logs()? {
            log.recover(id)?;
        }
        Ok(log)
    }

    /// What `open` cut off the end of logs, one entry per affected log. Empty when every log was clean.
    pub fn recovered(&self) -> &[TornTail] {
        &self.recovered
    }

    fn path(&self, log: &LogId) -> PathBuf {
        match log {
            LogId::Host => self.root.join(format!("host.{EXT}")),
            LogId::Session(id) => self.root.join("sessions").join(format!("{id}.{EXT}")),
        }
    }

    /// Every log file under the root. Files that can't be a log (wrong extension, invalid session id,
    /// directories) are ignored. A symlink named like a log is an error, never followed.
    fn stored_logs(&self) -> Result<Vec<LogId>, LogError> {
        let mut logs = Vec::new();
        let host = self.path(&LogId::Host);
        match fs::symlink_metadata(&host) {
            Ok(meta) => {
                refuse_symlink(&host, meta.file_type())?;
                if meta.is_file() {
                    logs.push(LogId::Host);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e)),
        }
        for entry in fs::read_dir(self.root.join("sessions")).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EXT) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let log = LogId::Session(SessionId::new(stem));
            if log.validate().is_err() {
                continue;
            }
            let file_type = entry.file_type().map_err(io_err)?;
            refuse_symlink(&path, file_type)?;
            if file_type.is_file() {
                logs.push(log);
            }
        }
        Ok(logs)
    }

    /// Indexes one log, truncating leftovers at its end.
    fn recover(&mut self, log: LogId) -> Result<(), LogError> {
        let path = self.path(&log);
        let mut file = open_existing(&path, OpenOptions::new().read(true).write(true))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io_err)?;
        let scan = scan(&log, &bytes, 1, Mode::Recover)?;
        let end = scan.valid_len as u64;
        let removed = bytes.len() as u64 - end;
        if removed > 0 {
            file.set_len(end).map_err(io_err)?;
            file.sync_all().map_err(io_err)?;
            self.recovered.push(TornTail {
                log: log.clone(),
                bytes: removed,
            });
        }
        let index = Index {
            starts: scan.starts,
            end,
            poisoned: false,
        };
        self.logs.insert(log, index);
        Ok(())
    }
}

impl EventLog for JsonlEventLog {
    fn append(
        &mut self,
        log: LogId,
        draft: FrameDraft,
        stamp: Stamp,
    ) -> Result<Envelope, LogError> {
        if LogId::of(&draft) != log {
            return Err(LogError::WrongLog {
                log,
                draft: draft.session,
            });
        }
        log.validate()?;
        let path = self.path(&log);
        let index = self.logs.entry(log.clone()).or_default();
        if index.poisoned {
            return Err(LogError::Poisoned { log });
        }

        let env = draft.seal(Seq::new(index.starts.len() as u64 + 1), stamp);
        let mut line = serde_json::to_vec(&env).map_err(|e| LogError::Io(e.to_string()))?;
        line.push(b'\n');

        let mut file = (self.open_append)(&path)?;
        let found = file.len().map_err(io_err)?;
        if found != index.end {
            return Err(LogError::ChangedOnDisk {
                log,
                expected: index.end,
                found,
            });
        }
        if let Err(e) = file.append(&line) {
            // Undo a partial write, so the next append doesn't land after a torn line.
            if file.set_len(index.end).is_err() {
                index.poisoned = true;
            }
            return Err(io_err(e));
        }
        index.starts.push(index.end);
        index.end += line.len() as u64;
        Ok(env)
    }

    fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, LogError> {
        self.read_batch(log, from, usize::MAX)
    }

    /// Seeks with the offset index to the first entry after `from`, and reads and parses only the next `max`.
    fn read_batch(&self, log: LogId, from: Seq, max: usize) -> Result<Vec<Envelope>, LogError> {
        log.validate()?;
        let Some(index) = self.logs.get(&log) else {
            return Ok(Vec::new());
        };
        let skip = usize::try_from(from.get()).unwrap_or(usize::MAX);
        let Some(&start) = index.starts.get(skip).filter(|_| max > 0) else {
            return Ok(Vec::new());
        };
        // Where the batch ends: the start of the entry after it, or the end of the log.
        let stop = index
            .starts
            .get(skip.saturating_add(max))
            .copied()
            .unwrap_or(index.end);

        let mut file = open_existing(&self.path(&log), OpenOptions::new().read(true))?;
        let found = file.metadata().map_err(io_err)?.len();
        // Bytes past `end` can only be a poisoned append's leftovers; they aren't entries.
        if found < index.end {
            return Err(LogError::ChangedOnDisk {
                log,
                expected: index.end,
                found,
            });
        }
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(start)).map_err(io_err)?;
        file.take(stop - start)
            .read_to_end(&mut bytes)
            .map_err(io_err)?;
        Ok(scan(&log, &bytes, from.get() + 1, Mode::Strict)?.entries)
    }

    fn logs(&self) -> Result<Vec<LogId>, LogError> {
        let with_entries = self.logs.iter().filter(|(_, i)| !i.starts.is_empty());
        Ok(with_entries.map(|(log, _)| log.clone()).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Reading a whole file on open: leftovers at the end are excluded from `valid_len`.
    Recover,
    /// Reading a region the index says holds only entries: everything must parse.
    Strict,
}

/// The entries parsed from part of a log, and how many leading bytes hold them.
struct Scan {
    entries: Vec<Envelope>,
    /// The offset of each entry within the scanned bytes.
    starts: Vec<u64>,
    valid_len: usize,
}

/// Parses `bytes`, whose first line should be seq `first_seq`.
fn scan(log: &LogId, bytes: &[u8], first_seq: u64, mode: Mode) -> Result<Scan, LogError> {
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let lines: Vec<&[u8]> = bytes[..complete].split_inclusive(|&b| b == b'\n').collect();

    let mut scan = Scan {
        entries: Vec::with_capacity(lines.len()),
        starts: Vec::with_capacity(lines.len()),
        valid_len: 0,
    };
    for (i, line) in lines.iter().enumerate() {
        let number = first_seq + i as u64;
        let corrupt = |reason: String| LogError::Corrupt {
            log: log.clone(),
            line: number,
            reason,
        };
        let env: Envelope = match serde_json::from_slice(line) {
            Ok(env) => env,
            Err(_) if mode == Mode::Recover && is_unwritten(&bytes[scan.valid_len..]) => break,
            Err(e) => return Err(corrupt(e.to_string())),
        };
        if env.seq() != Seq::new(number) {
            return Err(corrupt(format!(
                "seq {} where {number} was expected",
                env.seq()
            )));
        }
        if env.session() != log.session() {
            return Err(corrupt(format!("entry for session {:?}", env.session())));
        }
        scan.entries.push(env);
        scan.starts.push(scan.valid_len as u64);
        scan.valid_len += line.len();
    }
    // Bytes after the last newline are a torn write, whatever they hold.
    if mode == Mode::Strict && complete < bytes.len() {
        return Err(LogError::Corrupt {
            log: log.clone(),
            line: first_seq + lines.len() as u64,
            reason: "entry has no newline".into(),
        });
    }
    Ok(scan)
}

/// Whether `rest` is only NUL bytes and newlines: what power loss leaves where blocks were allocated but never
/// written.
fn is_unwritten(rest: &[u8]) -> bool {
    rest.iter().all(|&b| b == 0 || b == b'\n')
}

fn io_err(e: io::Error) -> LogError {
    LogError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    use serde_json::json;
    use shepherd_core::acpp::Seq;
    use shepherd_core::ports::{EventLog, LogError, LogId};
    use shepherd_testkit::contracts::event_log::{EventLogHarness, draft, session, stamp};
    use tempfile::TempDir;

    use super::{AppendFile, JsonlEventLog, TornTail};

    /// A fresh tempdir, chmodded 0700 so it passes the store's private-root check whatever the umask.
    fn private_tempdir() -> TempDir {
        let dir = TempDir::new().unwrap();
        chmod(dir.path(), 0o700);
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A log in a fresh tempdir. Reopening drops the old adapter and opens a new one on the same dir.
    fn temp_harness() -> EventLogHarness<JsonlEventLog> {
        let dir = private_tempdir();
        EventLogHarness::new(move |old| {
            drop(old);
            JsonlEventLog::open(dir.path()).unwrap()
        })
    }

    shepherd_testkit::event_log_contract!(temp_harness);

    fn try_push(log: &mut JsonlEventLog, id: &LogId, n: u64) -> Result<Seq, LogError> {
        log.append(id.clone(), draft(id, json!({ "n": n })), stamp(n))
            .map(|e| e.seq())
    }

    fn push(log: &mut JsonlEventLog, id: &LogId, n: u64) {
        try_push(log, id, n).unwrap();
    }

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn seqs(log: &JsonlEventLog, id: &LogId) -> Vec<u64> {
        let entries = log.read(id.clone(), Seq::ZERO).unwrap();
        entries.iter().map(|e| e.seq().get()).collect()
    }

    fn s1_path(dir: &Path) -> std::path::PathBuf {
        dir.join("sessions/s1.jsonl")
    }

    /// Opens `dir` with entries 1..=3 in session `s1`.
    fn three_entries(dir: &Path) -> JsonlEventLog {
        let mut log = JsonlEventLog::open(dir).unwrap();
        for n in 1..=3 {
            push(&mut log, &session("s1"), n);
        }
        log
    }

    /// Overwrites the first byte of line `line` (from 1) in place, so the file keeps its length.
    fn scribble_on_line(path: &Path, line: usize) {
        let mut bytes = fs::read(path).unwrap();
        let start = bytes
            .split_inclusive(|&b| b == b'\n')
            .take(line - 1)
            .map(<[u8]>::len)
            .sum::<usize>();
        bytes[start] = b'X';
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn read_batch_parses_only_the_entries_it_returns() {
        let dir = private_tempdir();
        let log = three_entries(dir.path());
        // Line 3 is unreadable now, but a batch that stops before it never parses it.
        scribble_on_line(&s1_path(dir.path()), 3);
        let batch = log.read_batch(session("s1"), Seq::ZERO, 2).unwrap();
        assert_eq!(
            batch.iter().map(|e| e.seq().get()).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_corrupt(log.read_batch(session("s1"), Seq::new(2), 1), 3);
    }

    fn assert_corrupt<T: std::fmt::Debug>(result: Result<T, LogError>, at: u64) {
        assert!(
            matches!(&result, Err(LogError::Corrupt { line, .. }) if *line == at),
            "expected corrupt at line {at}, got {result:?}"
        );
    }

    /// Run directly under the caller's umask, and again in child processes under extreme umasks by
    /// `modes_are_exact_whatever_the_umask`.
    #[test]
    fn open_creates_the_root_and_sessions_0700_and_files_0600() {
        let dir = private_tempdir();
        let root = dir.path().join("shepherd");
        let mut log = JsonlEventLog::open(&root).unwrap();
        push(&mut log, &LogId::Host, 1);
        push(&mut log, &session("s1"), 2);

        assert_eq!(mode(&root), 0o700, "root");
        assert_eq!(mode(&root.join("sessions")), 0o700, "sessions");
        assert_eq!(mode(&root.join("lock")), 0o600, "lock");
        assert_eq!(mode(&root.join("host.jsonl")), 0o600, "host log");
        assert_eq!(mode(&s1_path(&root)), 0o600, "session log");
    }

    /// The process umask is never changed (it's process-wide), so this reruns the test above in a child under a
    /// permissive umask (000) and one that strips owner bits (277): modes must come out exact either way.
    #[test]
    fn modes_are_exact_whatever_the_umask() {
        let exe = std::env::current_exe().unwrap();
        for umask in ["000", "277"] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "umask {umask} && exec \"$0\" --exact --test-threads=1 \"$1\""
                ))
                .arg(&exe)
                .arg("event_log::tests::open_creates_the_root_and_sessions_0700_and_files_0600")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "umask {umask}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn an_existing_root_open_to_group_or_others_is_refused_and_left_alone() {
        for open_mode in [0o755, 0o770, 0o701] {
            let dir = private_tempdir();
            let root = dir.path().join("shepherd");
            fs::create_dir(&root).unwrap();
            chmod(&root, open_mode);

            let result = JsonlEventLog::open(&root).map(|_| ());
            assert_eq!(
                result,
                Err(LogError::InsecurePermissions {
                    path: root.clone(),
                    mode: open_mode
                })
            );
            assert_eq!(mode(&root), open_mode);
            assert!(!root.join("lock").exists());
            assert!(!root.join("sessions").exists());
        }
    }

    fn assert_symlink_refused<T: std::fmt::Debug>(result: Result<T, LogError>) {
        assert!(
            matches!(&result, Err(LogError::Io(msg)) if msg.contains("symlink")),
            "{result:?}"
        );
    }

    /// Replaces `rel` under `dir` with a symlink to a private file or dir outside the store. A file target holds a
    /// secret with no trailing newline, which recovery would take for a torn tail if it followed the link.
    fn plant_symlink(dir: &Path, outside: &Path, rel: &str) -> std::path::PathBuf {
        let path = dir.join(rel);
        let target = outside.join("target");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            fs::create_dir(&target).unwrap();
            chmod(&target, 0o700);
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::write(&target, b"ghp_secrettoken").unwrap();
            chmod(&target, 0o600);
            let _ = fs::remove_file(&path);
        }
        std::os::unix::fs::symlink(&target, &path).unwrap();
        target
    }

    /// A store with entries in the host log and session `s1`.
    fn populated_store(dir: &Path) {
        let mut log = JsonlEventLog::open(dir).unwrap();
        push(&mut log, &LogId::Host, 1);
        push(&mut log, &session("s1"), 2);
    }

    /// A symlink anywhere in the store is refused on open, never followed: following one could truncate
    /// whatever file it points at as a "torn tail".
    #[test]
    fn a_symlinked_dir_lock_or_log_in_the_store_is_refused_on_open_and_its_target_untouched() {
        for rel in ["sessions", "lock", "host.jsonl", "sessions/s1.jsonl"] {
            let (dir, outside) = (private_tempdir(), private_tempdir());
            populated_store(dir.path());
            let target = plant_symlink(dir.path(), outside.path(), rel);

            assert_symlink_refused(JsonlEventLog::open(dir.path()).map(|_| ()));
            if target.is_file() {
                assert_eq!(fs::read(&target).unwrap(), b"ghp_secrettoken", "{rel}");
                assert_eq!(mode(&target), 0o600, "{rel}");
            } else {
                assert!(fs::read_dir(&target).unwrap().next().is_none(), "{rel}");
            }
        }
    }

    #[test]
    fn a_log_swapped_for_a_symlink_after_open_is_refused_by_read_and_append() {
        let (dir, outside) = (private_tempdir(), private_tempdir());
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &session("s1"), 1);
        let target = plant_symlink(dir.path(), outside.path(), "sessions/s1.jsonl");

        assert_symlink_refused(log.read(session("s1"), Seq::ZERO));
        assert_symlink_refused(try_push(&mut log, &session("s1"), 2));
        assert_eq!(fs::read(&target).unwrap(), b"ghp_secrettoken");
    }

    /// `link/` resolves through the link, so a trailing slash must not get a symlinked root past the check.
    #[test]
    fn a_symlinked_root_given_with_a_trailing_slash_is_still_refused() {
        let (dir, outside) = (private_tempdir(), private_tempdir());
        let root = dir.path().join("shepherd");
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();

        let mut with_slash = root.into_os_string();
        with_slash.push("/");
        assert_symlink_refused(JsonlEventLog::open(&with_slash).map(|_| ()));
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    /// A symlink planted where the root goes is refused even when it points at a private dir: it is never
    /// followed, and its target is never chmodded or written into.
    #[test]
    fn a_root_that_is_a_symlink_is_refused_and_not_followed() {
        for target_mode in [0o700, 0o755] {
            let dir = private_tempdir();
            let target = dir.path().join("elsewhere");
            fs::create_dir(&target).unwrap();
            chmod(&target, target_mode);
            let root = dir.path().join("shepherd");
            std::os::unix::fs::symlink(&target, &root).unwrap();

            let result = JsonlEventLog::open(&root).map(|_| ());
            assert!(
                matches!(&result, Err(LogError::Io(msg)) if msg.contains("symlink")),
                "{result:?}"
            );
            assert_eq!(mode(&target), target_mode);
            assert!(fs::read_dir(&target).unwrap().next().is_none());
            assert!(fs::symlink_metadata(&root).unwrap().is_symlink());
        }
    }

    /// Existing files and dirs inside the root are user data: one open to others is refused, never chmodded.
    #[test]
    fn existing_sessions_dir_lock_or_log_open_to_others_is_refused_and_left_alone() {
        for (rel, open_mode) in [
            ("sessions", 0o755),
            ("lock", 0o644),
            ("host.jsonl", 0o644),
            ("sessions/s1.jsonl", 0o640),
        ] {
            let dir = private_tempdir();
            let mut log = JsonlEventLog::open(dir.path()).unwrap();
            push(&mut log, &LogId::Host, 1);
            push(&mut log, &session("s1"), 2);
            drop(log);
            let path = dir.path().join(rel);
            chmod(&path, open_mode);

            let result = JsonlEventLog::open(dir.path()).map(|_| ());
            assert_eq!(
                result,
                Err(LogError::InsecurePermissions {
                    path: path.clone(),
                    mode: open_mode
                }),
                "{rel}"
            );
            assert_eq!(mode(&path), open_mode, "{rel}");
        }
    }

    /// A log file that appears after open (another writer, a restore) is checked before the first append to it.
    #[test]
    fn append_refuses_a_new_log_file_someone_else_created_open_to_others() {
        let dir = private_tempdir();
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        let path = s1_path(dir.path());
        fs::write(&path, b"").unwrap();
        chmod(&path, 0o644);

        assert_eq!(
            try_push(&mut log, &session("s1"), 1),
            Err(LogError::InsecurePermissions { path, mode: 0o644 })
        );
    }

    #[test]
    fn logs_are_host_jsonl_and_one_file_per_session() {
        let dir = private_tempdir();
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &LogId::Host, 1);
        push(&mut log, &session("s_1"), 2);
        push(&mut log, &session("s_1"), 3);

        let host = fs::read_to_string(dir.path().join("host.jsonl")).unwrap();
        let s1 = fs::read_to_string(dir.path().join("sessions/s_1.jsonl")).unwrap();
        assert_eq!(host.lines().count(), 1);
        assert_eq!(s1.lines().count(), 2);
        assert!(s1.ends_with('\n'));
    }

    #[test]
    fn open_creates_a_missing_root() {
        let dir = private_tempdir();
        let root = dir.path().join("nested/shepherd");
        let mut log = JsonlEventLog::open(&root).unwrap();
        push(&mut log, &session("s1"), 1);
        assert!(s1_path(&root).exists());
    }

    #[test]
    fn a_second_open_on_the_same_root_is_locked_until_the_first_is_dropped() {
        let dir = private_tempdir();
        let first = JsonlEventLog::open(dir.path()).unwrap();
        assert!(matches!(
            JsonlEventLog::open(dir.path()),
            Err(LogError::Locked)
        ));
        drop(first);
        JsonlEventLog::open(dir.path()).unwrap();
    }

    /// A child forked while the store is open shares the lock's file description until it execs. Dropping the
    /// store must release the lock anyway, or the next open in this process fails at random.
    #[test]
    fn dropping_the_store_releases_the_lock_even_if_a_child_inherited_it() {
        let dir = private_tempdir();
        let first = JsonlEventLog::open(dir.path()).unwrap();
        let inherited = first.lock.try_clone().unwrap();
        drop(first);
        JsonlEventLog::open(dir.path()).unwrap();
        drop(inherited);
    }

    #[test]
    fn torn_tail_without_newline_is_truncated_on_open_and_the_log_stays_usable() {
        let dir = private_tempdir();
        let s = session("s1");
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &s, 1);
        push(&mut log, &s, 2);
        drop(log);
        let torn = br#"{"acpp":1,"seq":3,"fra"#;
        append_raw(&s1_path(dir.path()), torn);

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(
            log.recovered(),
            [TornTail {
                log: s.clone(),
                bytes: torn.len() as u64
            }]
        );
        assert_eq!(seqs(&log, &s), [1, 2]);
        push(&mut log, &s, 3);
        drop(log);

        let log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered(), []);
        assert_eq!(seqs(&log, &s), [1, 2, 3]);
    }

    #[test]
    fn a_complete_entry_missing_its_newline_is_torn() {
        let dir = private_tempdir();
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &LogId::Host, 1);
        push(&mut log, &LogId::Host, 2);
        drop(log);
        let path = dir.path().join("host.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.trim_end_matches('\n')).unwrap();

        let log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered().len(), 1);
        assert_eq!(seqs(&log, &LogId::Host), [1]);
    }

    /// A complete line is never torn: truncating it could destroy data, so it's an error and the file is kept.
    #[test]
    fn an_unparseable_final_line_is_corrupt_on_open() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        append_raw(&s1_path(dir.path()), b"{not json\n");
        let before = fs::read(s1_path(dir.path())).unwrap();

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 4);
        assert_eq!(fs::read(s1_path(dir.path())).unwrap(), before);
    }

    /// An entry from a newer shepherd must survive a downgrade.
    #[test]
    fn a_final_entry_with_a_newer_envelope_version_is_corrupt_not_torn() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        let path = s1_path(dir.path());
        let text = fs::read_to_string(&path).unwrap();
        let last = text.lines().last().unwrap();
        let newer = last
            .replace("\"acpp\":1", "\"acpp\":2")
            .replace("\"seq\":3", "\"seq\":4");
        assert_ne!(newer, last);
        append_raw(&path, format!("{newer}\n").as_bytes());
        let before = fs::read(&path).unwrap();

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 4);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_trailing_region_of_nuls_and_newlines_is_torn_after_power_loss() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        let tail = b"\0\0\0\0\n\0\0\0\0";
        append_raw(&s1_path(dir.path()), tail);

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(
            log.recovered(),
            [TornTail {
                log: session("s1"),
                bytes: tail.len() as u64
            }]
        );
        push(&mut log, &session("s1"), 4);
        drop(log);
        let log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered(), []);
        assert_eq!(seqs(&log, &session("s1")), [1, 2, 3, 4]);
    }

    #[test]
    fn nuls_followed_by_a_valid_entry_are_corrupt() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        let path = s1_path(dir.path());
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        fs::write(&path, format!("{}\n\0\0\0\0\n{}\n", lines[0], lines[1])).unwrap();

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 2);
    }

    #[test]
    fn a_torn_only_entry_leaves_an_empty_usable_log() {
        let dir = private_tempdir();
        fs::create_dir(dir.path().join("sessions")).unwrap();
        chmod(&dir.path().join("sessions"), 0o700);
        fs::write(s1_path(dir.path()), b"{\"acpp\":").unwrap();
        chmod(&s1_path(dir.path()), 0o600);

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered().len(), 1);
        assert_eq!(log.logs().unwrap(), vec![]);
        push(&mut log, &session("s1"), 1);
        assert_eq!(seqs(&log, &session("s1")), [1]);
    }

    #[test]
    fn corruption_before_the_last_line_is_an_error_on_open() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        scribble_on_line(&s1_path(dir.path()), 2);

        let result = JsonlEventLog::open(dir.path());
        assert!(
            matches!(&result, Err(LogError::Corrupt { log, line: 2, .. }) if *log == session("s1")),
            "got {:?}",
            result.err()
        );
    }

    #[test]
    fn corruption_in_the_region_read_is_an_error_on_read() {
        let dir = private_tempdir();
        let log = three_entries(dir.path());
        scribble_on_line(&s1_path(dir.path()), 2);

        assert_corrupt(log.read(session("s1"), Seq::ZERO), 2);
        assert_corrupt(log.read(session("s1"), Seq::new(1)), 2);
    }

    /// Reads seek to the first wanted entry instead of re-parsing the whole log.
    #[test]
    fn read_from_a_seq_skips_the_entries_before_it() {
        let dir = private_tempdir();
        let log = three_entries(dir.path());
        scribble_on_line(&s1_path(dir.path()), 1);

        let after_one = log.read(session("s1"), Seq::new(1)).unwrap();
        assert_eq!(
            after_one.iter().map(|e| e.seq().get()).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn a_bad_line_followed_by_a_torn_tail_is_corrupt_not_torn() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        append_raw(&s1_path(dir.path()), b"{bad\n{\"ac");

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 4);
    }

    #[test]
    fn a_seq_gap_is_corrupt() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        let path = s1_path(dir.path());
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        fs::write(&path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 2);
    }

    #[test]
    fn an_entry_filed_under_the_wrong_log_is_corrupt() {
        let dir = private_tempdir();
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &session("a"), 1);
        push(&mut log, &session("a"), 2);
        drop(log);
        fs::copy(
            dir.path().join("sessions/a.jsonl"),
            dir.path().join("sessions/b.jsonl"),
        )
        .unwrap();

        assert_corrupt(JsonlEventLog::open(dir.path()).map(|_| ()), 1);
    }

    #[test]
    fn stray_files_next_to_the_logs_are_ignored() {
        let dir = private_tempdir();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(sessions.join("subdir.jsonl")).unwrap();
        chmod(&sessions, 0o700);
        fs::write(sessions.join(".DS_Store"), b"\0\0junk").unwrap();
        fs::write(sessions.join("notes.txt"), b"hello").unwrap();
        fs::write(sessions.join(".hidden.jsonl"), b"junk").unwrap();
        fs::write(sessions.join("Upper.jsonl"), b"junk").unwrap();

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.logs().unwrap(), vec![]);
        push(&mut log, &session("s1"), 1);
        assert_eq!(log.logs().unwrap(), vec![session("s1")]);
    }

    #[test]
    fn append_refuses_to_write_when_the_file_changed_on_disk() {
        let dir = private_tempdir();
        let mut log = three_entries(dir.path());
        let path = s1_path(dir.path());
        append_raw(&path, b"someone else\n");
        let before = fs::read(&path).unwrap();

        let result = try_push(&mut log, &session("s1"), 4);
        assert!(
            matches!(result, Err(LogError::ChangedOnDisk { .. })),
            "got {result:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn read_refuses_when_the_file_shrank_on_disk() {
        let dir = private_tempdir();
        let log = three_entries(dir.path());
        fs::write(s1_path(dir.path()), b"").unwrap();

        let result = log.read(session("s1"), Seq::ZERO);
        assert!(
            matches!(result, Err(LogError::ChangedOnDisk { .. })),
            "got {result:?}"
        );
    }

    /// A log file whose writes stop after a few bytes, and whose rollback can be made to fail too.
    struct FailingFile<const ROLLBACK_FAILS: bool>(File);

    impl<const ROLLBACK_FAILS: bool> AppendFile for FailingFile<ROLLBACK_FAILS> {
        fn len(&self) -> io::Result<u64> {
            self.0.len()
        }

        fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.0.write_all(&bytes[..5])?;
            Err(io::Error::other("disk full"))
        }

        fn set_len(&mut self, len: u64) -> io::Result<()> {
            if ROLLBACK_FAILS {
                Err(io::Error::other("rollback failed"))
            } else {
                self.0.set_len(len)
            }
        }
    }

    fn open_failing<const ROLLBACK_FAILS: bool>(
        path: &Path,
    ) -> Result<Box<dyn AppendFile>, LogError> {
        let file = super::open_append(path)?;
        Ok(Box::new(FailingFile::<ROLLBACK_FAILS>(file)))
    }

    #[test]
    fn a_failed_append_that_rolls_back_leaves_the_log_unchanged_and_usable() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));
        let before = fs::read(s1_path(dir.path())).unwrap();

        let mut log = JsonlEventLog::open_with(dir.path(), open_failing::<false>).unwrap();
        assert!(matches!(
            try_push(&mut log, &session("s1"), 4),
            Err(LogError::Io(_))
        ));
        assert!(matches!(
            try_push(&mut log, &session("s1"), 4),
            Err(LogError::Io(_))
        ));
        assert_eq!(fs::read(s1_path(dir.path())).unwrap(), before);
        assert_eq!(seqs(&log, &session("s1")), [1, 2, 3]);
        drop(log);

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered(), []);
        assert_eq!(try_push(&mut log, &session("s1"), 4), Ok(Seq::new(4)));
    }

    #[test]
    fn a_failed_append_that_cannot_roll_back_poisons_the_log_until_reopen() {
        let dir = private_tempdir();
        drop(three_entries(dir.path()));

        let mut log = JsonlEventLog::open_with(dir.path(), open_failing::<true>).unwrap();
        assert!(matches!(
            try_push(&mut log, &session("s1"), 4),
            Err(LogError::Io(_))
        ));
        assert_eq!(
            try_push(&mut log, &session("s1"), 4),
            Err(LogError::Poisoned { log: session("s1") })
        );
        // Reads are unaffected.
        assert_eq!(seqs(&log, &session("s1")), [1, 2, 3]);
        drop(log);

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(
            log.recovered(),
            [TornTail {
                log: session("s1"),
                bytes: 5
            }]
        );
        assert_eq!(try_push(&mut log, &session("s1"), 4), Ok(Seq::new(4)));
    }
}
