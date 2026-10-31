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
//!
//! What capture refuses (an error, and nothing is archived):
//! - a relative path, or one with a `..` component;
//! - a path that is not under the agent home (canonicalized, so the home may be reached through symlinks), or not in
//!   the place that agent keeps that session's transcript (the file name must name the session);
//! - a symlink, or anything but a directory, at any component below the home, and a symlink or non-regular file at
//!   the transcript or any subagent transcript;
//! - more bytes in all than [`CaptureLimits::max_bytes`], or more files than [`CaptureLimits::max_files`]. A file is
//!   read at most one byte past what is left of the budget, so an oversized file is never read whole.

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
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// One transcript file, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captured {
    pub role: FileRole,
    /// Where it was read: the canonical agent home joined with the payload's path below it.
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
/// that session's transcript under `home`. `read` counts every byte read from a transcript file.
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
    let main = home.join(&rel);
    let main_meta = walk(&home, &parts)?;
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
    let mut out = vec![Captured {
        role,
        content: budget.read(&main, &main_meta)?,
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
            if !matches!(
                Path::new(session.as_str()).components().collect::<Vec<_>>()[..],
                [Component::Normal(c)] if c == OsStr::new(session.as_str())
            ) {
                return false;
            }
            let file = format!("{session}.jsonl");
            matches!(parts, [projects, _project, name]
                if *projects == OsStr::new("projects") && *name == OsStr::new(&file))
        }
        AgentKind::Codex => match parts {
            [sessions, _dirs @ .., name] if *sessions == OsStr::new("sessions") => {
                rollout_thread(name) == Some(session.as_str())
            }
            _ => false,
        },
    }
}

/// The thread id a Codex rollout file name carries: `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread>[_<rollout>].jsonl`, as
/// `RolloutFileName::parse` reads it.
fn rollout_thread(name: &OsStr) -> Option<&str> {
    let core = name
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let timestamp = core.get(..19)?;
    jiff::civil::DateTime::strptime("%Y-%m-%dT%H-%M-%S", timestamp).ok()?;
    let ids = core.get(19..)?.strip_prefix('-')?;
    let thread = ids.split_once('_').map_or(ids, |(thread, _)| thread);
    (!thread.is_empty()).then_some(thread)
}

/// Checks every component of `home/parts` without following symlinks: each directory a real directory, the last a
/// regular file. Returns the file's `lstat`.
fn walk(home: &Path, parts: &[&OsStr]) -> Result<Metadata, CaptureError> {
    let mut at = home.to_path_buf();
    let Some((file, dirs)) = parts.split_last() else {
        return Err(CaptureError::WrongType(at));
    };
    for dir in dirs {
        at.push(dir);
        let meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
        check_dir(&at, &meta)?;
    }
    at.push(file);
    let meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
    check_file(&at, &meta)?;
    Ok(meta)
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
    // A symlink swapped in above the directory while it was listed would change where the entries came from.
    unchanged_path(&dir)?;
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
        let mut content = Vec::new();
        let n = file
            .take(self.left.saturating_add(1))
            .read_to_end(&mut content)
            .map_err(|e| io(path, e))?;
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.read.fetch_add(n, Ordering::Relaxed);
        if n > self.left {
            return Err(CaptureError::TooLarge {
                path: path.to_path_buf(),
                max: self.max,
            });
        }
        self.left -= n;
        Ok(content)
    }
}
