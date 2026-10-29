//! ACP++ logs as JSONL files, one envelope per line (see "Storage" in `docs/acpp.md`):
//!
//! ```text
//! <root>/host.jsonl
//! <root>/sessions/<session-id>.jsonl
//! ```
//!
//! An entry is durable once its line, newline included, is written; each append is a single write of the whole
//! line. [`JsonlEventLog::open`] scans every log: a torn final line (bytes after the last newline, or a final
//! line that doesn't parse) is truncated and reported by [`JsonlEventLog::recovered`], so the runtime can warn.
//! Anything else unreadable, a seq gap, or an entry for another log is [`LogError::Corrupt`].
//!
//! One adapter per root: it caches each log's length, so another writer on the same files would break seq
//! numbering. Within a process the `&mut self` on append already serializes writers.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use shepherd_core::acpp::{Envelope, FrameDraft, Seq, Stamp};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LogError, LogId};

const EXT: &str = "jsonl";

/// A torn final line that [`JsonlEventLog::open`] cut off a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TornTail {
    pub log: LogId,
    /// How many bytes were removed.
    pub bytes: u64,
}

pub struct JsonlEventLog {
    root: PathBuf,
    /// The number of entries in each log that has any, which is also its last seq.
    lens: BTreeMap<LogId, u64>,
    recovered: Vec<TornTail>,
}

impl JsonlEventLog {
    /// Opens (creating if needed) the logs under `root`, truncating torn final lines.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LogError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("sessions")).map_err(io_err)?;
        let mut log = Self {
            root,
            lens: BTreeMap::new(),
            recovered: Vec::new(),
        };
        for id in log.stored_logs()? {
            log.recover(id)?;
        }
        Ok(log)
    }

    /// The torn tails `open` truncated, one per affected log. Empty when every log was clean.
    pub fn recovered(&self) -> &[TornTail] {
        &self.recovered
    }

    fn path(&self, log: &LogId) -> PathBuf {
        match log {
            LogId::Host => self.root.join(format!("host.{EXT}")),
            LogId::Session(id) => self.root.join("sessions").join(format!("{id}.{EXT}")),
        }
    }

    /// Every log file under the root. Files that can't be a log (wrong extension, unsafe name, directories)
    /// are ignored.
    fn stored_logs(&self) -> Result<Vec<LogId>, LogError> {
        let mut logs = Vec::new();
        if self.path(&LogId::Host).is_file() {
            logs.push(LogId::Host);
        }
        for entry in fs::read_dir(self.root.join("sessions")).map_err(io_err)? {
            let path = entry.map_err(io_err)?.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some(EXT) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let log = LogId::Session(SessionId::new(stem));
            if log.validate().is_ok() {
                logs.push(log);
            }
        }
        Ok(logs)
    }

    fn recover(&mut self, log: LogId) -> Result<(), LogError> {
        let path = self.path(&log);
        let bytes = fs::read(&path).map_err(io_err)?;
        let scan = scan(&log, &bytes)?;
        if scan.valid_len < bytes.len() {
            let file = OpenOptions::new().write(true).open(&path).map_err(io_err)?;
            file.set_len(scan.valid_len as u64).map_err(io_err)?;
            file.sync_all().map_err(io_err)?;
            self.recovered.push(TornTail {
                log: log.clone(),
                bytes: (bytes.len() - scan.valid_len) as u64,
            });
        }
        if !scan.entries.is_empty() {
            self.lens.insert(log, scan.entries.len() as u64);
        }
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
        let seq = self.lens.get(&log).copied().unwrap_or(0) + 1;
        let env = draft.seal(Seq::new(seq), stamp);
        let mut line = serde_json::to_vec(&env).map_err(|e| LogError::Io(e.to_string()))?;
        line.push(b'\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(&log))
            .map_err(io_err)?;
        write_line(&mut file, &line).map_err(io_err)?;
        self.lens.insert(log, seq);
        Ok(env)
    }

    fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, LogError> {
        log.validate()?;
        let bytes = match fs::read(self.path(&log)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(e)),
        };
        let mut entries = scan(&log, &bytes)?.entries;
        entries.retain(|e| e.seq() > from);
        Ok(entries)
    }

    fn logs(&self) -> Result<Vec<LogId>, LogError> {
        Ok(self.lens.keys().cloned().collect())
    }
}

/// Writes `line` in one call. If that fails part-way, cuts the file back so no torn line is left in front of
/// the next append.
fn write_line(file: &mut File, line: &[u8]) -> io::Result<()> {
    let len = file.metadata()?.len();
    let written = file.write_all(line).and_then(|()| file.flush());
    if written.is_err() {
        let _ = file.set_len(len);
    }
    written
}

/// A log file's readable entries, and how many leading bytes hold them.
struct Scan {
    entries: Vec<Envelope>,
    valid_len: usize,
}

/// Parses a log file. A torn final line is left out of `valid_len`; any other bad line is corrupt.
fn scan(log: &LogId, bytes: &[u8]) -> Result<Scan, LogError> {
    // Bytes after the last newline are a torn write, whatever they hold.
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let has_tail = complete < bytes.len();
    let lines: Vec<&[u8]> = bytes[..complete].split_inclusive(|&b| b == b'\n').collect();

    let mut entries = Vec::with_capacity(lines.len());
    let mut valid_len = 0;
    for (i, line) in lines.iter().enumerate() {
        let number = i as u64 + 1;
        let corrupt = |reason: String| LogError::Corrupt {
            log: log.clone(),
            line: number,
            reason,
        };
        let env: Envelope = match serde_json::from_slice(line) {
            Ok(env) => env,
            // Only the very last thing in the file can be a torn write.
            Err(_) if i + 1 == lines.len() && !has_tail => break,
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
        entries.push(env);
        valid_len += line.len();
    }
    Ok(Scan { entries, valid_len })
}

fn io_err(e: io::Error) -> LogError {
    LogError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use serde_json::json;
    use shepherd_core::acpp::Seq;
    use shepherd_core::ports::{EventLog, LogError, LogId};
    use shepherd_testkit::contracts::event_log::{EventLogHarness, draft, session, stamp};
    use tempfile::TempDir;

    use super::{JsonlEventLog, TornTail};

    /// A log in a fresh tempdir. Reopening drops the old adapter and opens a new one on the same dir.
    fn temp_harness() -> EventLogHarness<JsonlEventLog> {
        let dir = TempDir::new().unwrap();
        EventLogHarness::new(move |old| {
            drop(old);
            JsonlEventLog::open(dir.path()).unwrap()
        })
    }

    shepherd_testkit::event_log_contract!(temp_harness);

    fn push(log: &mut JsonlEventLog, id: &LogId, n: u64) {
        log.append(id.clone(), draft(id, json!({ "n": n })), stamp(n))
            .unwrap();
    }

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn seqs(log: &JsonlEventLog, id: &LogId) -> Vec<u64> {
        let entries = log.read(id.clone(), Seq::ZERO).unwrap();
        entries.iter().map(|e| e.seq().get()).collect()
    }

    #[test]
    fn logs_are_host_jsonl_and_one_file_per_session() {
        let dir = TempDir::new().unwrap();
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
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("nested/shepherd");
        let mut log = JsonlEventLog::open(&root).unwrap();
        push(&mut log, &session("s1"), 1);
        assert!(root.join("sessions/s1.jsonl").exists());
    }

    #[test]
    fn torn_tail_without_newline_is_truncated_on_open_and_the_log_stays_usable() {
        let dir = TempDir::new().unwrap();
        let s = session("s1");
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &s, 1);
        push(&mut log, &s, 2);
        drop(log);
        let torn = br#"{"acpp":1,"seq":3,"fra"#;
        append_raw(&dir.path().join("sessions/s1.jsonl"), torn);

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
        let dir = TempDir::new().unwrap();
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

    #[test]
    fn unparseable_final_line_is_truncated_on_open() {
        let dir = TempDir::new().unwrap();
        let s = session("s1");
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &s, 1);
        drop(log);
        append_raw(&dir.path().join("sessions/s1.jsonl"), b"{not json\n");

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(
            log.recovered(),
            [TornTail {
                log: s.clone(),
                bytes: 10
            }]
        );
        push(&mut log, &s, 2);
        assert_eq!(seqs(&log, &s), [1, 2]);
    }

    #[test]
    fn a_torn_only_entry_leaves_an_empty_usable_log() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("sessions")).unwrap();
        fs::write(dir.path().join("sessions/s1.jsonl"), b"{\"acpp\":").unwrap();

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.recovered().len(), 1);
        assert_eq!(log.logs().unwrap(), vec![]);
        push(&mut log, &session("s1"), 1);
        assert_eq!(seqs(&log, &session("s1")), [1]);
    }

    fn corrupt_line_two(dir: &Path) {
        let path = dir.join("sessions/s1.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "{garbage";
        fs::write(&path, lines.join("\n") + "\n").unwrap();
    }

    fn three_entries(dir: &Path) -> JsonlEventLog {
        let mut log = JsonlEventLog::open(dir).unwrap();
        for n in 1..=3 {
            push(&mut log, &session("s1"), n);
        }
        log
    }

    #[test]
    fn corruption_before_the_last_line_is_an_error_on_open() {
        let dir = TempDir::new().unwrap();
        drop(three_entries(dir.path()));
        corrupt_line_two(dir.path());

        let result = JsonlEventLog::open(dir.path());
        assert!(
            matches!(&result, Err(LogError::Corrupt { log, line: 2, .. }) if *log == session("s1")),
            "got {:?}",
            result.err()
        );
    }

    #[test]
    fn corruption_before_the_last_line_is_an_error_on_read() {
        let dir = TempDir::new().unwrap();
        let log = three_entries(dir.path());
        corrupt_line_two(dir.path());

        let result = log.read(session("s1"), Seq::ZERO);
        assert!(
            matches!(&result, Err(LogError::Corrupt { line: 2, .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn a_bad_line_followed_by_a_torn_tail_is_corrupt_not_torn() {
        let dir = TempDir::new().unwrap();
        drop(three_entries(dir.path()));
        append_raw(&dir.path().join("sessions/s1.jsonl"), b"{bad\n{\"ac");

        let result = JsonlEventLog::open(dir.path());
        assert!(
            matches!(&result, Err(LogError::Corrupt { line: 4, .. })),
            "got {:?}",
            result.err()
        );
    }

    #[test]
    fn a_seq_gap_is_corrupt() {
        let dir = TempDir::new().unwrap();
        drop(three_entries(dir.path()));
        let path = dir.path().join("sessions/s1.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        fs::write(&path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();

        let result = JsonlEventLog::open(dir.path());
        assert!(
            matches!(&result, Err(LogError::Corrupt { line: 2, .. })),
            "got {:?}",
            result.err()
        );
    }

    #[test]
    fn an_entry_filed_under_the_wrong_log_is_corrupt() {
        let dir = TempDir::new().unwrap();
        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        push(&mut log, &session("a"), 1);
        push(&mut log, &session("a"), 2);
        drop(log);
        fs::copy(
            dir.path().join("sessions/a.jsonl"),
            dir.path().join("sessions/b.jsonl"),
        )
        .unwrap();

        let result = JsonlEventLog::open(dir.path());
        assert!(
            matches!(&result, Err(LogError::Corrupt { line: 1, .. })),
            "got {:?}",
            result.err()
        );
    }

    #[test]
    fn stray_files_next_to_the_logs_are_ignored() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(sessions.join("subdir.jsonl")).unwrap();
        fs::write(sessions.join(".DS_Store"), b"\0\0junk").unwrap();
        fs::write(sessions.join("notes.txt"), b"hello").unwrap();
        fs::write(sessions.join(".hidden.jsonl"), b"junk").unwrap();

        let mut log = JsonlEventLog::open(dir.path()).unwrap();
        assert_eq!(log.logs().unwrap(), vec![]);
        push(&mut log, &session("s1"), 1);
        assert_eq!(log.logs().unwrap(), vec![session("s1")]);
    }
}
