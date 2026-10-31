//! Restore: putting a session's archived transcripts back where its agent's resume looks for them, for a given cwd and
//! agent home (T41). The caller resolves the agent home; this crate never reads `$HOME` or the environment.
//!
//! Where each agent looks (provisional, from the agents' docs and source; T44 re-checks against the real agents):
//! - Claude Code (<https://code.claude.com/docs/en/sessions>, "Where transcripts are stored" and "Resume by session
//!   ID or name", read 2026-10-30): `projects/<project>/<session-id>.jsonl` under its config dir, "where `<project>`
//!   is your working directory path with non-alphanumeric characters replaced by `-`" (the docs' example:
//!   `/home/user/work/my-repo` is `-home-user-work-my-repo`, <https://code.claude.com/docs/en/claude-directory>,
//!   "Clear local data"). Subagent transcripts go in `projects/<project>/<session-id>/subagents/`, under the names
//!   they were captured with. Two cases the docs leave open are refused rather than guessed: a name over 200
//!   characters (Claude Code "truncates the name to 200 characters and appends a hash of the full path", hash
//!   unspecified) and a cwd that isn't ASCII (which characters count as alphanumeric is unspecified).
//!   `claude --resume <session-id>` looks in the current project dir first, then every other project.
//! - Codex (openai/codex rust-v0.166.0, `codex-rs/rollout/src/recorder.rs`, `rollout_file_name.rs`): under
//!   `CODEX_HOME`, `sessions/<YYYY>/<MM>/<DD>/<rollout file name>`, the date being the one the file name carries.
//!   Resuming by id (`find_thread_path_by_id_str`, `codex-rs/rollout/src/list.rs`) walks `sessions/` by file name
//!   and doesn't filter by cwd.
//!
//! Transcript content is restored byte for byte. Neither agent's docs or source (above) say resume checks the `cwd`
//! recorded inside a transcript, so nothing is rewritten; T44 settles that against the real agents (decision 0020).
//!
//! The cwd is used as given (normalised), so the caller passes the path the agent will see as its working directory.
//!
//! How files are written:
//! - Everything is checked before anything is written: every directory below the (canonicalized) agent home must be
//!   a real directory, never a symlink, and an existing file at a target must be a regular file, not a symlink. An
//!   existing file with the same content is left as it is; one with different content is [`RestoreError::Exists`] and
//!   nothing is written at all.
//! - Missing directories are created `0700` and files `0600`, set through a descriptor so a umask can't change them.
//!   Existing directories are never chmodded.
//! - Each file is written to a new temp file (`create_new`, so never through a symlink) in its target directory and
//!   synced; only when every temp is written are they published, each with a hard link to its final name, which fails
//!   rather than replace anything there. A failure at any point removes the temps and every file this restore
//!   published, so a final path holds either a complete file or what was there before. Directories this restore
//!   created are left (empty).
//! - Before and after each publish the target directory must still resolve to itself ([`RestoreError::Changed`]
//!   otherwise). Known gap, as in capture: without `openat` a same-user race can still swap a directory for a symlink
//!   between that check and the link.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::capture::{is_one_component, rollout_name};
use crate::ids::{AgentKind, AgentSessionId, FileRole, SessionId, SnapshotId};
use crate::index::{ReadFile, Session};
use crate::private::{create_new_private_file, normalize};

/// With the `fault-injection-pause-before-restore-publish` feature (tests only, decision 0015), a restore of the agent
/// session with this id writes [`PAUSE_BEFORE_PUBLISH_MARKER`] and a newline to stdout once all its temp files are
/// written, before it publishes any of them, then waits for a byte (or EOF) on stdin.
#[cfg(feature = "fault-injection-pause-before-restore-publish")]
pub const PAUSE_BEFORE_PUBLISH_SESSION: &str = "shepherd-archive-test-pause-before-publish";
#[cfg(feature = "fault-injection-pause-before-restore-publish")]
pub const PAUSE_BEFORE_PUBLISH_MARKER: &str = "shepherd-archive-paused-before-restore-publish";

const DIR_MODE: u32 = 0o700;

/// The longest Claude Code project dir name that is the plain encoding of the cwd.
const MAX_CLAUDE_PROJECT_LEN: usize = 200;

/// Names temp files uniquely within this process; the pid separates processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What the caller needs to resume a restored session: run `agent` in `cwd`, resuming its session `agent_session_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub agent: AgentKind,
    pub agent_session_id: AgentSessionId,
    pub cwd: PathBuf,
}

/// Why a restore wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    #[error("{0} is not an absolute path")]
    NotAbsolute(PathBuf),
    #[error("{0} has a `..` component")]
    ParentDir(PathBuf),
    #[error("session {0} has no snapshot to restore")]
    NoSnapshot(SessionId),
    /// The agent's documented layout doesn't say where its transcripts go for this cwd.
    #[error("can't restore for the cwd {cwd}: {reason}")]
    UnsupportedCwd { cwd: PathBuf, reason: &'static str },
    /// The snapshot's files don't fit where the session's agent keeps them.
    #[error("snapshot {snapshot} can't be restored: {reason}")]
    BadSnapshot {
        snapshot: SnapshotId,
        reason: String,
    },
    /// A different file is already at a target path. It is never overwritten.
    #[error("{0} already exists with different content; not overwriting it")]
    Exists(PathBuf),
    #[error("{0} is a symlink; refusing to follow it")]
    Symlink(PathBuf),
    #[error("{0} is the wrong kind of file")]
    WrongType(PathBuf),
    /// A directory on a target's path stopped resolving to itself (a symlink swapped in) during the restore.
    #[error("{0} changed while it was being restored")]
    Changed(PathBuf),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

fn io(path: &Path, e: std::io::Error) -> RestoreError {
    RestoreError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// One file to restore: its directories below the agent home, its name and its content.
#[derive(Debug)]
struct Target<'a> {
    dirs: Vec<OsString>,
    name: OsString,
    content: &'a [u8],
}

/// Refuses a relative path or one with a `..` component, and returns it normalised.
pub(crate) fn checked_path(path: &Path) -> Result<PathBuf, RestoreError> {
    if !path.is_absolute() {
        return Err(RestoreError::NotAbsolute(path.to_path_buf()));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(RestoreError::ParentDir(path.to_path_buf()));
    }
    Ok(normalize(path))
}

/// Restores `files`, the content of `session`'s snapshot `snapshot`, for `cwd` under `home` (both from
/// [`checked_path`]).
pub(crate) fn restore_files(
    session: &Session,
    snapshot: SnapshotId,
    files: &[ReadFile],
    cwd: PathBuf,
    home: &Path,
) -> Result<Resume, RestoreError> {
    let targets = match session.agent {
        AgentKind::Claude => claude_targets(&session.agent_session_id, snapshot, files, &cwd)?,
        AgentKind::Codex => codex_targets(&session.agent_session_id, snapshot, files)?,
    };
    #[cfg(feature = "fault-injection-pause-before-restore-publish")]
    let pause = session.agent_session_id.as_str() == PAUSE_BEFORE_PUBLISH_SESSION;
    #[cfg(not(feature = "fault-injection-pause-before-restore-publish"))]
    let pause = false;
    write_targets(home, &targets, pause)?;
    Ok(Resume {
        agent: session.agent,
        agent_session_id: session.agent_session_id.clone(),
        cwd,
    })
}

fn bad(snapshot: SnapshotId, reason: impl Into<String>) -> RestoreError {
    RestoreError::BadSnapshot {
        snapshot,
        reason: reason.into(),
    }
}

/// Claude Code's project dir name for `cwd`: every character but an ASCII letter or digit replaced by `-`.
fn claude_project(cwd: &Path) -> Result<String, RestoreError> {
    let unsupported = |reason| RestoreError::UnsupportedCwd {
        cwd: cwd.to_path_buf(),
        reason,
    };
    let text = cwd.to_str().filter(|s| s.is_ascii()).ok_or_else(|| {
        unsupported("Claude Code's docs don't say how it encodes characters outside ASCII")
    })?;
    let name: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if name.len() > MAX_CLAUDE_PROJECT_LEN {
        return Err(unsupported(
            "Claude Code shortens a project dir name over 200 characters with a hash its docs don't specify",
        ));
    }
    Ok(name)
}

/// The one file in `files` with `role`, refusing any file with a role in `refused`.
fn single<'a>(
    snapshot: SnapshotId,
    files: &'a [ReadFile],
    role: FileRole,
    refused: &[FileRole],
) -> Result<&'a ReadFile, RestoreError> {
    if let Some(f) = files.iter().find(|f| refused.contains(&f.file.role)) {
        return Err(bad(
            snapshot,
            format!(
                "it has a {} file, which its agent doesn't write",
                f.file.role
            ),
        ));
    }
    let mut found = files.iter().filter(|f| f.file.role == role);
    match (found.next(), found.next()) {
        (Some(f), None) => Ok(f),
        (None, _) => Err(bad(snapshot, format!("it has no {role} file"))),
        (Some(_), Some(_)) => Err(bad(snapshot, format!("it has more than one {role} file"))),
    }
}

fn claude_targets<'a>(
    session: &AgentSessionId,
    snapshot: SnapshotId,
    files: &'a [ReadFile],
    cwd: &Path,
) -> Result<Vec<Target<'a>>, RestoreError> {
    let id = session.as_str();
    if !is_one_component(id) {
        return Err(bad(
            snapshot,
            format!("the session id {id:?} is not one path component"),
        ));
    }
    let main = single(snapshot, files, FileRole::Main, &[FileRole::Rollout])?;
    let project = OsString::from(claude_project(cwd)?);
    let project_dirs = vec![OsString::from("projects"), project];
    let mut targets = vec![Target {
        dirs: project_dirs.clone(),
        name: OsString::from(format!("{id}.jsonl")),
        content: &main.content,
    }];
    let mut subagent_dirs = project_dirs;
    subagent_dirs.extend([OsString::from(id), OsString::from("subagents")]);
    let mut names = BTreeSet::new();
    for f in files.iter().filter(|f| f.file.role == FileRole::Subagent) {
        let path = &f.file.original_path;
        let name = path
            .file_name()
            .filter(|n| is_subagent_name(n))
            .ok_or_else(|| {
                bad(
                    snapshot,
                    format!("{} is not a subagent transcript name", path.display()),
                )
            })?;
        if !names.insert(name) {
            return Err(bad(
                snapshot,
                format!("two subagent transcripts are named {}", name.display()),
            ));
        }
        targets.push(Target {
            dirs: subagent_dirs.clone(),
            name: name.to_os_string(),
            content: &f.content,
        });
    }
    Ok(targets)
}

/// A plain `<something>.jsonl` file name that isn't hidden.
fn is_subagent_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| {
        is_one_component(n)
            && !n.starts_with('.')
            && n.len() > ".jsonl".len()
            && n.ends_with(".jsonl")
    })
}

fn codex_targets<'a>(
    session: &AgentSessionId,
    snapshot: SnapshotId,
    files: &'a [ReadFile],
) -> Result<Vec<Target<'a>>, RestoreError> {
    let rollout = single(
        snapshot,
        files,
        FileRole::Rollout,
        &[FileRole::Main, FileRole::Subagent],
    )?;
    let path = &rollout.file.original_path;
    let name = path.file_name().unwrap_or_default();
    let at = match rollout_name(name) {
        Some((at, thread)) if thread == session.as_str() => at,
        _ => {
            return Err(bad(
                snapshot,
                format!(
                    "{} is not a rollout file name for the thread {session}",
                    path.display()
                ),
            ));
        }
    };
    Ok(vec![Target {
        dirs: vec![
            OsString::from("sessions"),
            OsString::from(format!("{:04}", at.year())),
            OsString::from(format!("{:02}", at.month())),
            OsString::from(format!("{:02}", at.day())),
        ],
        name: name.to_os_string(),
        content: &rollout.content,
    }])
}

fn check_dir(path: &Path, meta: &Metadata) -> Result<(), RestoreError> {
    if meta.file_type().is_symlink() {
        return Err(RestoreError::Symlink(path.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(RestoreError::WrongType(path.to_path_buf()));
    }
    Ok(())
}

/// Whether the regular file at `path`, whose `lstat` is `meta`, holds exactly `content`. Refuses a symlink or anything
/// but a regular file (checked before opening it, so a FIFO can't block), and a file swapped while it was opened.
fn holds(path: &Path, meta: &Metadata, content: &[u8]) -> Result<bool, RestoreError> {
    if meta.file_type().is_symlink() {
        return Err(RestoreError::Symlink(path.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(RestoreError::WrongType(path.to_path_buf()));
    }
    let file = File::open(path).map_err(|e| io(path, e))?;
    let opened = file.metadata().map_err(|e| io(path, e))?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(RestoreError::Changed(path.to_path_buf()));
    }
    let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
    let mut existing = Vec::new();
    file.take(len.saturating_add(1))
        .read_to_end(&mut existing)
        .map_err(|e| io(path, e))?;
    Ok(existing == content)
}

/// Whether `target` still needs writing: `false` if the same content is already there. Checks every directory on its
/// path that exists, and the file if it does.
fn needs_writing(home: &Path, target: &Target<'_>) -> Result<bool, RestoreError> {
    let mut at = home.to_path_buf();
    for dir in &target.dirs {
        at.push(dir);
        match fs::symlink_metadata(&at) {
            Ok(meta) => check_dir(&at, &meta)?,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(io(&at, e)),
        }
    }
    at.push(&target.name);
    match fs::symlink_metadata(&at) {
        Ok(meta) if holds(&at, &meta, target.content)? => Ok(false),
        Ok(_) => Err(RestoreError::Exists(at)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
        Err(e) => Err(io(&at, e)),
    }
}

/// Creates each missing directory of `dirs` below `home` exactly `0700`, checking existing ones are real directories.
/// Returns the last one.
fn make_dirs(home: &Path, dirs: &[OsString]) -> Result<PathBuf, RestoreError> {
    let mut at = home.to_path_buf();
    for dir in dirs {
        at.push(dir);
        match fs::DirBuilder::new().mode(DIR_MODE).create(&at) {
            Ok(()) => {
                // Ours: set the exact mode through a descriptor, after checking it is the directory just made.
                let opened = File::open(&at).map_err(|e| io(&at, e))?;
                let fd_meta = opened.metadata().map_err(|e| io(&at, e))?;
                let path_meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
                check_dir(&at, &path_meta)?;
                if (fd_meta.dev(), fd_meta.ino()) != (path_meta.dev(), path_meta.ino()) {
                    return Err(RestoreError::Changed(at));
                }
                opened
                    .set_permissions(fs::Permissions::from_mode(DIR_MODE))
                    .map_err(|e| io(&at, e))?;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&at).map_err(|e| io(&at, e))?;
                check_dir(&at, &meta)?;
            }
            Err(e) => return Err(io(&at, e)),
        }
    }
    Ok(at)
}

/// Fails if `dir` (the canonical home joined with plain components) no longer resolves to itself.
fn unchanged(dir: &Path) -> Result<(), RestoreError> {
    let resolved = fs::canonicalize(dir).map_err(|e| io(dir, e))?;
    if resolved != dir {
        return Err(RestoreError::Changed(dir.to_path_buf()));
    }
    Ok(())
}

/// A file written to a temp name, waiting to be published.
struct Staged<'a> {
    dir: PathBuf,
    temp: PathBuf,
    target: &'a Target<'a>,
    /// The temp file's inode: the published file is the same one.
    ino: (u64, u64),
    published: bool,
}

/// Writes `content` to a new `0600` temp file in `dir` named after `name`, and syncs it.
fn write_temp<'a>(dir: PathBuf, target: &'a Target<'a>) -> Result<Staged<'a>, RestoreError> {
    loop {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(&target.name);
        temp_name.push(format!(".{}.{n}.tmp", std::process::id()));
        let temp = dir.join(temp_name);
        let mut file = match create_new_private_file(&temp) {
            Ok(file) => file,
            // A stale temp from a crashed process with the same pid: pick another name.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(&temp, e)),
        };
        let written = file
            .write_all(target.content)
            .and_then(|()| file.sync_all())
            .and_then(|()| file.metadata());
        return match written {
            Ok(meta) => Ok(Staged {
                dir,
                temp,
                target,
                ino: (meta.dev(), meta.ino()),
                published: false,
            }),
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(io(&temp, e))
            }
        };
    }
}

/// Writes every target that needs it under `home` (module docs): all or nothing.
fn write_targets(home: &Path, targets: &[Target<'_>], pause: bool) -> Result<(), RestoreError> {
    let home = fs::canonicalize(home).map_err(|e| io(home, e))?;
    let mut todo = Vec::new();
    for target in targets {
        if needs_writing(&home, target)? {
            todo.push(target);
        }
    }
    let mut staged = Vec::new();
    let outcome = stage_and_publish(&home, &todo, &mut staged, pause);
    for s in &staged {
        let _ = fs::remove_file(&s.temp);
    }
    if outcome.is_err() {
        for s in staged.iter().filter(|s| s.published) {
            let path = s.dir.join(&s.target.name);
            // Only the file this restore published, never one that replaced it since.
            if let Ok(meta) = fs::symlink_metadata(&path)
                && (meta.dev(), meta.ino()) == s.ino
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
    for dir in staged.iter().map(|s| &s.dir).collect::<BTreeSet<_>>() {
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
    outcome
}

fn stage_and_publish<'a>(
    home: &Path,
    todo: &[&'a Target<'a>],
    staged: &mut Vec<Staged<'a>>,
    pause: bool,
) -> Result<(), RestoreError> {
    for target in todo {
        let dir = make_dirs(home, &target.dirs)?;
        staged.push(write_temp(dir, target)?);
    }
    if pause {
        pause_before_publish();
    }
    for s in staged.iter_mut() {
        unchanged(&s.dir)?;
        let path = s.dir.join(&s.target.name);
        match fs::hard_link(&s.temp, &path) {
            Ok(()) => s.published = true,
            // Something appeared there since the check: fine only if it is the same content.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
                if !holds(&path, &meta, s.target.content)? {
                    return Err(RestoreError::Exists(path));
                }
            }
            Err(e) => return Err(io(&path, e)),
        }
        unchanged(&s.dir)?;
    }
    Ok(())
}

#[cfg(feature = "fault-injection-pause-before-restore-publish")]
fn pause_before_publish() {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{PAUSE_BEFORE_PUBLISH_MARKER}").and_then(|()| out.flush());
    let _ = std::io::stdin().read(&mut [0u8; 1]);
}

#[cfg(not(feature = "fault-injection-pause-before-restore-publish"))]
fn pause_before_publish() {}
