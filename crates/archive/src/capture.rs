//! Capture: reading exactly the transcript files an agent wrote for one of its sessions, from the path its hook payload
//! gave (T40). Never a path shepherd computed: the payload's `transcript_path` is checked, not derived.
//!
//! Where each agent writes (provisional, from the agents' docs and source; T44 re-checks against the real agents):
//! - Claude Code (<https://code.claude.com/docs/en/claude-directory>, "Application data", and
//!   <https://code.claude.com/docs/en/hooks>, read 2026-10-30): under its config dir (`~/.claude`, or
//!   `CLAUDE_CONFIG_DIR`), `projects/<project>/<session>.jsonl`, and its subagents' transcripts in
//!   `projects/<project>/<session>/subagents/*.jsonl`. Hook payloads carry the main file as `transcript_path`.
//! - Codex (openai/codex rust-v0.166.0: `codex-rs/rollout/src/recorder.rs`, `rollout_file_name.rs`, `lib.rs`; the
//!   hook input structs in `codex-rs/hooks/src/schema.rs`): under `CODEX_HOME`,
//!   `sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<thread id>.jsonl` (a reverted thread appends
//!   `_<rollout id>`). The hook `session_id` is the thread id and `transcript_path` is that rollout file, or null.
//!   Codex compresses a rollout untouched for 7 days in place (`codex-rs/rollout/src/compression.rs`:
//!   `MIN_ROLLOUT_AGE`, `COMPRESSED_SUFFIX`, `compress_rollout_if_cold_blocking`): one zstd frame (level 3, the content
//!   size pledged) at the same path plus `.zst`, `rollout-...jsonl.zst`, and the plain file is removed. Codex prefers
//!   the plain file when both are there, and decompresses a compressed one back to plain before appending to it.
//!   So capture reads the plain rollout if it is there, else its `.zst` (whichever of the two the payload named),
//!   decompressing it through the budget below, and archives the rollout it holds under the plain name: a restore
//!   writes a plain rollout, and an unchanged rollout stores no new blob whether or not it was compressed since. A
//!   plain rollout that vanishes between being chosen and being opened (Codex compressing it then) is chosen again,
//!   once.
//!
//! What capture refuses (an error, and nothing is archived):
//! - a relative path, or one with a `..` component;
//! - a path that is not under the agent home (canonicalized, so the home may be reached through symlinks), or not in
//!   the place that agent keeps that session's transcript (the file name must name the session);
//! - a symlink, or anything but a directory, at any component below the home, and a symlink or non-regular file at
//!   the transcript or any subagent transcript;
//! - more bytes in all than [`CaptureLimits::max_bytes`], or more files than [`CaptureLimits::max_files`]. A file is
//!   read at most one byte past what is left of the budget, so an oversized file is never read whole. A compressed
//!   rollout counts what it inflates to, and is inflated at most one byte past what is left; a compressed file that is
//!   itself bigger than what is left is refused unread;
//! - a compressed rollout that isn't valid zstd, or asks for a bigger window than Codex ever uses
//!   ([`CaptureError::Corrupt`]).
//!
//! Each file is opened only after its path is checked, and refused if what was opened isn't the file that was checked
//! or its path no longer resolves to itself (a symlink swapped in on the way). Known gap: a FIFO swapped in between
//! the check and the open can still block that open, since std can't open without following or with `O_NONBLOCK`.
//! Closing it needs `openat`/`O_NONBLOCK`, i.e. `libc` or `rustix` as a dependency; it takes a same-user race.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::Timestamp;

use crate::blob::DEFAULT_MAX_BLOB_LEN;
use crate::ids::{AgentKind, AgentSessionId, CommitHash, FileRole, SessionId, SnapshotReason};
use crate::index::SessionCommit;
use crate::private::normalize;

/// With the `fault-injection-pause-before-transcript-open` feature (tests only, decision 0015), a capture for the agent
/// session with this id writes [`PAUSE_BEFORE_OPEN_MARKER`] and a newline to stdout after a transcript's path is checked
/// and before the file is opened, then waits for a byte (or EOF) on stdin.
#[cfg(feature = "fault-injection-pause-before-transcript-open")]
pub const PAUSE_BEFORE_OPEN_SESSION: &str = "shepherd-archive-test-pause-before-open";
#[cfg(feature = "fault-injection-pause-before-transcript-open")]
pub const PAUSE_BEFORE_OPEN_MARKER: &str = "shepherd-archive-paused-before-transcript-open";

/// A snapshot to take of a session's transcripts: [`crate::Archive::capture`].
#[derive(Debug, Clone)]
pub struct Capture<'a> {
    /// The recorded session. Its agent and agent session id say which transcript `transcript_path` must be.
    pub session: SessionId,
    pub captured_at: Timestamp,
    pub head_commit: Option<CommitHash>,
    pub reason: SnapshotReason,
    pub commits: Vec<SessionCommit>,
    /// The agent's home: Claude Code's config dir (`~/.claude`, or `CLAUDE_CONFIG_DIR`), or Codex's `CODEX_HOME`.
    /// The caller resolves it; this crate never reads `$HOME` or the environment.
    pub agent_home: &'a Path,
    /// The `transcript_path` of the agent's hook payload, as given.
    pub transcript_path: &'a Path,
    pub limits: CaptureLimits,
}

/// How much one capture may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLimits {
    /// The most bytes, over all the session's transcript files together.
    pub max_bytes: u64,
    /// The most files: the main transcript (or rollout) and every subagent transcript.
    pub max_files: usize,
}

impl Default for CaptureLimits {
    /// The blob store's cap (so every captured file can be read back) and 1024 files.
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BLOB_LEN,
            max_files: 1024,
        }
    }
}

/// Why a capture read nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("{0} is not an absolute path")]
    NotAbsolute(PathBuf),
    #[error("{0} has a `..` component")]
    ParentDir(PathBuf),
    /// The transcript path is not under the agent home.
    #[error("{path} is outside the agent home {home}")]
    Outside { path: PathBuf, home: PathBuf },
    /// The path is under the agent home but not where that agent keeps this session's transcript.
    #[error("{path} is not where {agent} keeps the transcript of its session {session}")]
    NotTheSessionsTranscript {
        path: PathBuf,
        agent: AgentKind,
        session: AgentSessionId,
    },
    /// The path is under the agent home but not where that agent keeps any session's transcript.
    #[error("{path} is not where {agent} keeps a transcript")]
    NotATranscript { path: PathBuf, agent: AgentKind },
    #[error("{0} is a symlink; refusing to follow it")]
    Symlink(PathBuf),
    #[error("{0} is the wrong kind of file")]
    WrongType(PathBuf),
    /// The file was replaced, or a symlink appeared on its path, while it was opened.
    #[error("{0} changed while it was being opened")]
    Changed(PathBuf),
    #[error("reading {path} would take the capture past its {max}-byte limit")]
    TooLarge { path: PathBuf, max: u64 },
    #[error("the session's transcripts are more than {max} files (in {dir})")]
    TooManyFiles { dir: PathBuf, max: usize },
    /// A compressed rollout that doesn't decompress.
    #[error("{path} is not a valid zstd file: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// One transcript file, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captured {
    pub role: FileRole,
    /// Where it was read: the canonical agent home joined with the payload's path below it. For a rollout read from
    /// its compressed `.zst`, the plain rollout's path, which is what its content is.
    pub path: PathBuf,
    pub content: Vec<u8>,
}

fn io(path: &Path, e: std::io::Error) -> CaptureError {
    CaptureError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Reads `agent`'s transcripts for its session `session` from `transcript_path` (the hook payload's), which must be
/// that session's transcript under `home`. `read` counts every byte read from a transcript file (for a compressed
/// rollout, every byte it inflated to).
pub(crate) fn read_transcripts(
    agent: AgentKind,
    session: &AgentSessionId,
    home: &Path,
    transcript_path: &Path,
    limits: CaptureLimits,
    read: &AtomicU64,
) -> Result<Vec<Captured>, CaptureError> {
    let (home, rel) = below_home(home, transcript_path)?;
    let parts: Vec<&OsStr> = rel.iter().collect();
    if !in_agent_layout(agent, session, &parts) {
        return Err(CaptureError::NotTheSessionsTranscript {
            path: transcript_path.to_path_buf(),
            agent,
            session: session.clone(),
        });
    }
    let (main, main_file) = choose_main(agent, &home, &rel, &parts)?;
    let mut budget = Budget {
        left: limits.max_bytes,
        max: limits.max_bytes,
        read,
        #[cfg(feature = "fault-injection-pause-before-transcript-open")]
        pause: session.as_str() == PAUSE_BEFORE_OPEN_SESSION,
    };
    let subagents = match agent {
        AgentKind::Claude => claude_subagents(&main, session)?,
        AgentKind::Codex => Vec::new(),
    };
    if subagents.len().saturating_add(1) > limits.max_files {
        return Err(CaptureError::TooManyFiles {
            dir: main.parent().unwrap_or(&home).to_path_buf(),
            max: limits.max_files,
        });
    }
    let role = match agent {
        AgentKind::Claude => FileRole::Main,
        AgentKind::Codex => FileRole::Rollout,
    };
    let content = read_main(agent, &home, &parts, &main, &main_file, &mut budget)?;
    let mut out = vec![Captured {
        role,
        content,
        path: main,
    }];
    for (path, meta) in subagents {
        out.push(Captured {
            role: FileRole::Subagent,
            content: budget.read(&path, &meta)?,
            path,
        });
    }
    Ok(out)
}

/// Reads the main transcript `agent` keeps at `transcript_path` (the hook payload's) under `home`, whichever of its
/// sessions it is: checked as [`read_transcripts`] checks it, within `limits`. No subagent transcripts.
pub(crate) fn read_main_transcript(
    agent: AgentKind,
    home: &Path,
    transcript_path: &Path,
    limits: CaptureLimits,
) -> Result<Vec<u8>, CaptureError> {
    let (home, rel) = below_home(home, transcript_path)?;
    let parts: Vec<&OsStr> = rel.iter().collect();
    let session = session_named(agent, &parts).filter(|s| in_agent_layout(agent, s, &parts));
    if session.is_none() {
        return Err(CaptureError::NotATranscript {
            path: transcript_path.to_path_buf(),
            agent,
        });
    }
    let (main, main_file) = choose_main(agent, &home, &rel, &parts)?;
    let read = AtomicU64::new(0);
    let mut budget = Budget {
        left: limits.max_bytes,
        max: limits.max_bytes,
        read: &read,
        #[cfg(feature = "fault-injection-pause-before-transcript-open")]
        pause: false,
    };
    read_main(agent, &home, &parts, &main, &main_file, &mut budget)
}

/// The agent session whose transcript the file at `parts` (below the home) would be, by its name: Claude Code's
/// `<session>.jsonl`, Codex's rollout name. `None` for a name that names none.
fn session_named(agent: AgentKind, parts: &[&OsStr]) -> Option<AgentSessionId> {
    let name = parts.last()?;
    let session = match agent {
        AgentKind::Claude => name.to_str()?.strip_suffix(".jsonl")?,
        AgentKind::Codex => rollout_thread(plain_rollout_name(name))?,
    };
    session.parse().ok()
}

/// The main transcript's path at `home/rel` (whose components are `parts`), and the file holding it, each component
/// checked: for Codex, the plain rollout or its compressed `.zst`.
fn choose_main(
    agent: AgentKind,
    home: &Path,
    rel: &Path,
    parts: &[&OsStr],
) -> Result<(PathBuf, OnDisk), CaptureError> {
    match agent {
        AgentKind::Claude => {
            let meta = walk(home, parts)?;
            let main = home.join(rel);
            Ok((main.clone(), OnDisk::Plain(main, meta)))
        }
        AgentKind::Codex => codex_rollout(home, parts),
    }
}

/// Reads the main transcript `main_file` (chosen as `main`) within `budget`.
fn read_main(
    agent: AgentKind,
    home: &Path,
    parts: &[&OsStr],
    main: &Path,
    main_file: &OnDisk,
    budget: &mut Budget<'_>,
) -> Result<Vec<u8>, CaptureError> {
    match budget.read_on_disk(main_file) {
        // Codex compressed the rollout after it was chosen and before it was opened (it writes the `.zst`, then
        // removes the plain file): choose again, once, rather than call a conversation that is there missing.
        Err(CaptureError::Io { .. })
            if agent == AgentKind::Codex
                && matches!(main_file, OnDisk::Plain(..))
                && is_absent(main) =>
        {
            let (_, again) = codex_rollout(home, parts)?;
            budget.read_on_disk(&again)
        }
        read => read,
    }
}

/// The canonical `home`, and `path` relative to it: only normal components, at least one.
fn below_home(home: &Path, path: &Path) -> Result<(PathBuf, PathBuf), CaptureError> {
    for p in [home, path] {
        if !p.is_absolute() {
            return Err(CaptureError::NotAbsolute(p.to_path_buf()));
        }
        if p.components().any(|c| c == Component::ParentDir) {
            return Err(CaptureError::ParentDir(p.to_path_buf()));
        }
    }
    let given = normalize(home);
    let canonical = fs::canonicalize(&given).map_err(|e| io(&given, e))?;
    let path = normalize(path);
    // The payload may spell the home either way: as the agent was given it, or resolved.
    let rel = path
        .strip_prefix(&canonical)
        .or_else(|_| path.strip_prefix(&given))
        .ok()
        .filter(|rel| {
            !rel.as_os_str().is_empty()
                && rel.components().all(|c| matches!(c, Component::Normal(_)))
        })
        .map(Path::to_path_buf);
    match rel {
        Some(rel) => Ok((canonical, rel)),
        None => Err(CaptureError::Outside {
            path,
            home: canonical,
        }),
    }
}

/// Whether `parts` (below the agent home) is where `agent` keeps session `session`'s transcript.
fn in_agent_layout(agent: AgentKind, session: &AgentSessionId, parts: &[&OsStr]) -> bool {
    match agent {
        AgentKind::Claude => {
            // The session id also names its subagents' directory, so it must be one plain path component.
            if !is_one_component(session.as_str()) {
                return false;
            }
            let file = format!("{session}.jsonl");
            matches!(parts, [projects, _project, name]
                if *projects == OsStr::new("projects") && *name == OsStr::new(&file))
        }
        AgentKind::Codex => match parts {
            [sessions, _dirs @ .., name] if *sessions == OsStr::new("sessions") => {
                rollout_thread(plain_rollout_name(name)) == Some(session.as_str())
            }
            _ => false,
        },
    }
}

/// Whether `name` is exactly one plain path component: not empty, `.` or `..`, and without a `/`.
pub(crate) fn is_one_component(name: &str) -> bool {
    matches!(
        Path::new(name).components().collect::<Vec<_>>()[..],
        [Component::Normal(c)] if c == OsStr::new(name)
    )
}

/// The thread id a Codex rollout file name carries: `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread>[_<rollout>].jsonl`, as
/// `RolloutFileName::parse` reads it.
fn rollout_thread(name: &OsStr) -> Option<&str> {
    rollout_name(name).map(|(_, thread)| thread)
}

/// The time and thread id a Codex rollout file name carries (see [`rollout_thread`]).
pub(crate) fn rollout_name(name: &OsStr) -> Option<(jiff::civil::DateTime, &str)> {
    let core = name
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let timestamp = core.get(..19)?;
    let at = jiff::civil::DateTime::strptime("%Y-%m-%dT%H-%M-%S", timestamp).ok()?;
    let ids = core.get(19..)?.strip_prefix('-')?;
    let thread = ids.split_once('_').map_or(ids, |(thread, _)| thread);
    (!thread.is_empty()).then_some((at, thread))
}

/// What Codex compresses a rollout to: its name plus this (`COMPRESSED_SUFFIX` in
/// `codex-rs/rollout/src/compression.rs`).
const COMPRESSED_SUFFIX: &str = ".zst";

/// The biggest zstd window (log2 of its bytes) a compressed rollout may ask the decoder for: a frame asking for more is
/// [`CaptureError::Corrupt`], refused before the window is allocated. Codex compresses at level 3, whose window is at
/// most 2^21 bytes (zstd 1.5.7 `lib/compress/clevels.h`, the level-3 row of the table for inputs over 256 KB; smaller
/// inputs get smaller windows). 2^23 (8 MiB) leaves room for a change of level.
const MAX_WINDOW_LOG: u32 = 23;

/// The plain rollout name a compressed one (`rollout-...jsonl.zst`) stands for; any other name as it is.
fn plain_rollout_name(name: &OsStr) -> &OsStr {
    name.as_bytes()
        .strip_suffix(COMPRESSED_SUFFIX.as_bytes())
        .filter(|plain| plain.ends_with(b".jsonl"))
        .map_or(name, OsStr::from_bytes)
}

/// A transcript file found on disk, with its `lstat`.
enum OnDisk {
    Plain(PathBuf, Metadata),
    /// A rollout Codex compressed in place.
    Zstd(PathBuf, Metadata),
}

/// The plain rollout's path at `home/parts` (the last part named either way), and the file holding it: the plain
/// rollout if it is there, else its compressed `.zst`. Every component is checked as [`walk`] checks it. Neither there
/// is an error naming the plain one.
fn codex_rollout(home: &Path, parts: &[&OsStr]) -> Result<(PathBuf, OnDisk), CaptureError> {
    let Some((name, dirs)) = parts.split_last() else {
        return Err(CaptureError::WrongType(home.to_path_buf()));
    };
    let dir = walk_dirs(home, dirs)?;
    let plain = dir.join(plain_rollout_name(name));
    match fs::symlink_metadata(&plain) {
        Ok(meta) => {
            check_file(&plain, &meta)?;
            Ok((plain.clone(), OnDisk::Plain(plain, meta)))
        }
        Err(missing) if missing.kind() == ErrorKind::NotFound => {
            let mut compressed = plain.clone().into_os_string();
            compressed.push(COMPRESSED_SUFFIX);
            let compressed = PathBuf::from(compressed);
            match fs::symlink_metadata(&compressed) {
                Ok(meta) => {
                    check_file(&compressed, &meta)?;
                    Ok((plain, OnDisk::Zstd(compressed, meta)))
                }
                Err(e) if e.kind() == ErrorKind::NotFound => Err(io(&plain, missing)),
                Err(e) => Err(io(&compressed, e)),
            }
        }
        Err(e) => Err(io(&plain, e)),
    }
}

/// Checks every component of `home/parts` without following symlinks: each directory a real directory, the last a
/// regular file. Returns the file's `lstat`.
fn walk(home: &Path, parts: &[&OsStr]) -> Result<Metadata, CaptureError> {
    let Some((file, dirs)) = parts.split_last() else {
        return Err(CaptureError::WrongType(home.to_path_buf()));
    };
    let mut at = walk_dirs(home, dirs)?;
    at.push(file);
    let meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
    check_file(&at, &meta)?;
    Ok(meta)
}

/// Checks each of `home/dirs` without following symlinks, each a real directory. Returns the last.
fn walk_dirs(home: &Path, dirs: &[&OsStr]) -> Result<PathBuf, CaptureError> {
    let mut at = home.to_path_buf();
    for dir in dirs {
        at.push(dir);
        let meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
        check_dir(&at, &meta)?;
    }
    Ok(at)
}

fn check_dir(path: &Path, meta: &Metadata) -> Result<(), CaptureError> {
    if meta.file_type().is_symlink() {
        return Err(CaptureError::Symlink(path.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(CaptureError::WrongType(path.to_path_buf()));
    }
    Ok(())
}

fn check_file(path: &Path, meta: &Metadata) -> Result<(), CaptureError> {
    if meta.file_type().is_symlink() {
        return Err(CaptureError::Symlink(path.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(CaptureError::WrongType(path.to_path_buf()));
    }
    Ok(())
}

/// Nothing is at `path` (a symlink counts as something, and isn't followed).
fn is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(e) if e.kind() == ErrorKind::NotFound)
}

/// The directory at `path`, if there is one: `None` if nothing is there, an error for a symlink or anything else.
fn optional_dir(path: &Path) -> Result<Option<()>, CaptureError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => check_dir(path, &meta).map(Some),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

/// Claude Code's subagent transcripts for the session whose main transcript is `main`: the `*.jsonl` files in
/// `<session>/subagents/` next to it, by name. Directories there are skipped; a symlink is refused.
fn claude_subagents(
    main: &Path,
    session: &AgentSessionId,
) -> Result<Vec<(PathBuf, Metadata)>, CaptureError> {
    let Some(project) = main.parent() else {
        return Ok(Vec::new());
    };
    let session_dir = project.join(session.as_str());
    if optional_dir(&session_dir)?.is_none() {
        return Ok(Vec::new());
    }
    let dir = session_dir.join("subagents");
    if optional_dir(&dir)?.is_none() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
        let entry = entry.map_err(|e| io(&dir, e))?;
        let name = entry.file_name();
        if !name.as_bytes().ends_with(b".jsonl") {
            continue;
        }
        let path = dir.join(&name);
        let meta = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
        if meta.is_dir() {
            continue;
        }
        check_file(&path, &meta)?;
        found.push((path, meta));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Fails if `path` (canonical home joined with plain components) no longer resolves to itself: some component became a
/// symlink.
fn unchanged_path(path: &Path) -> Result<(), CaptureError> {
    let resolved = fs::canonicalize(path).map_err(|e| io(path, e))?;
    if resolved != path {
        return Err(CaptureError::Changed(path.to_path_buf()));
    }
    Ok(())
}

/// What is left of a capture's byte limit.
struct Budget<'a> {
    left: u64,
    max: u64,
    read: &'a AtomicU64,
    #[cfg(feature = "fault-injection-pause-before-transcript-open")]
    pause: bool,
}

impl Budget<'_> {
    /// Reads the regular file at `path`, whose `lstat` was `checked`, taking its bytes from the budget. Reads at most
    /// one byte more than is left.
    fn read(&mut self, path: &Path, checked: &Metadata) -> Result<Vec<u8>, CaptureError> {
        let file = self.open(path, checked)?;
        let mut content = Vec::new();
        let n = file
            .take(self.left.saturating_add(1))
            .read_to_end(&mut content)
            .map_err(|e| io(path, e))?;
        self.take(path, n)?;
        Ok(content)
    }

    /// Reads the zstd file at `path`, whose `lstat` was `checked`, and decompresses it, taking what it inflates to
    /// from the budget. Inflates at most one byte more than is left, and refuses unread a file bigger than what is
    /// left.
    fn inflate(&mut self, path: &Path, checked: &Metadata) -> Result<Vec<u8>, CaptureError> {
        let file = self.open(path, checked)?;
        if checked.len() > self.left {
            return Err(self.too_large(path));
        }
        let mut decoder =
            zstd::stream::read::Decoder::new(file.take(self.left)).map_err(|e| io(path, e))?;
        decoder
            .window_log_max(MAX_WINDOW_LOG)
            .map_err(|e| io(path, e))?;
        let mut content = Vec::new();
        let n = decoder
            .take(self.left.saturating_add(1))
            .read_to_end(&mut content)
            .map_err(|e| CaptureError::Corrupt {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        self.take(path, n)?;
        Ok(content)
    }

    /// Reads `file`, inflating it if it is compressed.
    fn read_on_disk(&mut self, file: &OnDisk) -> Result<Vec<u8>, CaptureError> {
        match file {
            OnDisk::Plain(path, meta) => self.read(path, meta),
            OnDisk::Zstd(path, meta) => self.inflate(path, meta),
        }
    }

    /// Opens the regular file at `path`, whose `lstat` was `checked`, refusing it if it isn't that file any more.
    fn open(&self, path: &Path, checked: &Metadata) -> Result<File, CaptureError> {
        #[cfg(feature = "fault-injection-pause-before-transcript-open")]
        if self.pause {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{PAUSE_BEFORE_OPEN_MARKER}").and_then(|()| out.flush());
            let _ = std::io::stdin().read(&mut [0u8; 1]);
        }
        let file = File::open(path).map_err(|e| io(path, e))?;
        let meta = file.metadata().map_err(|e| io(path, e))?;
        if (meta.dev(), meta.ino()) != (checked.dev(), checked.ino()) || !meta.is_file() {
            return Err(CaptureError::Changed(path.to_path_buf()));
        }
        unchanged_path(path)?;
        Ok(file)
    }

    /// Takes `n` bytes, just read from `path`, from the budget: refused if that is more than is left.
    fn take(&mut self, path: &Path, n: usize) -> Result<(), CaptureError> {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.read.fetch_add(n, Ordering::Relaxed);
        if n > self.left {
            return Err(self.too_large(path));
        }
        self.left -= n;
        Ok(())
    }

    fn too_large(&self, path: &Path) -> CaptureError {
        CaptureError::TooLarge {
            path: path.to_path_buf(),
            max: self.max,
        }
    }
}
