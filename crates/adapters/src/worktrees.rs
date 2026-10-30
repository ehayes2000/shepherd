//! `Worktrees` over the `git` CLI. Every call runs `git -C <dir> ...` and parses only porcelain or plumbing output.
//!
//! # Never waiting on a person
//!
//! Each git process starts a new session (`setsid`), so it has no controlling terminal and neither do its children:
//! ssh can't open `/dev/tty` to ask for a passphrase or host key. Stdin is closed, and the environment turns off the
//! prompts that don't need a terminal: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` (Git Credential Manager) and
//! `SSH_ASKPASS_REQUIRE=never`. A process that runs past its timeout is killed with its whole process group, hooks and
//! ssh included. Output goes to temp files rather than pipes, so a background process left behind by a hook can't hold
//! a call open. A `worktree add` killed this way is undone: its worktree and new branch are removed (see
//! [`Worktrees::create`]).
//!
//! # The user's config
//!
//! In production git runs with the user's real config, so every option that changes what we observe or do is pinned
//! on the command line:
//!
//! - `status`: `--untracked-files=all --ignore-submodules=none` (beats `status.showUntrackedFiles`,
//!   `diff.ignoreSubmodules`, `submodule.<name>.ignore`). `remove` checks this status itself before calling
//!   `git worktree remove`, so untracked files are never deleted without `force`.
//! - `fetch`: an explicit refspec `+refs/heads/<default>:refs/remotes/origin/<default>` (beats a custom or
//!   single-branch `remote.origin.fetch`), `--no-tags --no-prune --no-recurse-submodules --no-auto-gc` (beats
//!   `remote.origin.tagOpt`, `fetch.prune`, `fetch.pruneTags`, `fetch.recurseSubmodules`, `gc.auto`).
//! - `worktree add`: `--no-track --no-guess-remote` (beats `branch.autoSetupMerge`, `worktree.guessRemote`).
//! - Config injected through the environment (`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and its
//!   `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`) is removed, as are variables that point git at another repo
//!   (`GIT_DIR`, ...).
//!
//! Left to the user on purpose: hooks (`core.hooksPath`, `post-checkout` on `worktree add`), ssh and credential
//! settings. Nothing here pushes, so `push.default` doesn't matter.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shepherd_core::ports::worktrees::{
    managed_parts, normalize_lexically, repo_name_from_common_dir,
};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, Created, RepoRef, Worktree, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
};
use wait_timeout::ChildExt;

/// For every git call except fetch. Generous, because `worktree add` checks out files and runs the user's hooks.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Variables that would point git at some other repo than the `-C` directory (e.g. when shepherd itself runs from a
/// git hook), or inject config.
const REMOVED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];
/// Prefixes of the numbered variables that go with `GIT_CONFIG_COUNT`.
const REMOVED_ENV_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

pub struct GitWorktrees {
    root: PathBuf,
    /// `Some(vars)`: git sees only `PATH` plus `vars`. `None`: the inherited environment, cleaned.
    isolated_env: Option<Vec<(OsString, OsString)>>,
    git_timeout: Duration,
    fetch_timeout: Duration,
    /// For the git calls that undo a timed-out `worktree add`. Separate from `git_timeout`, so cleanup still has room
    /// when that timeout was set short.
    cleanup_timeout: Duration,
    /// Serializes fetch + `worktree add`: concurrent fetches into one repo race on ref locks.
    create_lock: Mutex<()>,
}

impl GitWorktrees {
    /// Manages worktrees under `root` (`~/.shepherd/worktrees` in production).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            isolated_env: None,
            git_timeout: GIT_TIMEOUT,
            fetch_timeout: FETCH_TIMEOUT,
            cleanup_timeout: GIT_TIMEOUT,
            create_lock: Mutex::new(()),
        }
    }

    /// Runs git with only `PATH` and `env`, instead of the inherited environment (tests use it to isolate git from
    /// the user's machine).
    pub fn with_isolated_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.isolated_env = Some(env);
        self
    }

    /// The timeout for a fetch. One that runs past it counts as a failed fetch.
    pub fn with_fetch_timeout(mut self, timeout: Duration) -> Self {
        self.fetch_timeout = timeout;
        self
    }

    /// The timeout for every other git call. One that runs past it is [`WorktreeError::Timeout`].
    pub fn with_git_timeout(mut self, timeout: Duration) -> Self {
        self.git_timeout = timeout;
        self
    }

    /// The timeout for the git calls that clean up after a timed-out `worktree add`.
    pub fn with_cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.cleanup_timeout = timeout;
        self
    }

    fn git<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<String, Failure> {
        self.git_within(dir, args, self.git_timeout)
    }

    fn git_within<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Duration,
    ) -> Result<String, Failure> {
        let io = |e: io::Error| Failure::Io(e.to_string());
        let mut stdout = tempfile::tempfile().map_err(io)?;
        let mut stderr = tempfile::tempfile().map_err(io)?;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().map_err(io)?)
            .stderr(stderr.try_clone().map_err(io)?);
        match &self.isolated_env {
            Some(env) => {
                command.env_clear();
                if let Some(path) = std::env::var_os("PATH") {
                    command.env("PATH", path);
                }
                command.envs(env.iter().map(|(k, v)| (k, v)));
            }
            None => {
                for (key, _) in std::env::vars_os() {
                    let key_str = key.to_string_lossy();
                    if REMOVED_ENV.contains(&key_str.as_ref())
                        || REMOVED_ENV_PREFIXES.iter().any(|p| key_str.starts_with(p))
                    {
                        command.env_remove(&key);
                    }
                }
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_OPTIONAL_LOCKS", "0");
        // SAFETY: `setsid` is async-signal-safe, and the closure touches nothing else.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(io)?;
        drop(command);
        let waited = child.wait_timeout(timeout);
        if !matches!(waited, Ok(Some(_))) {
            kill_group(child.id());
            // Kill the group again once git is dead but not yet reaped, for a hook it forked while the first kill
            // was being delivered. Until git is reaped its pid (the group id) can't be reused by another process.
            wait_unreaped(child.id());
            kill_group(child.id());
            let _ = child.wait();
            // git's own children (e.g. the `git branch` that `worktree add -b` runs) may still be dying, and a
            // syscall in flight, such as renaming a ref lock into place, completes first. Callers that clean up
            // after a timeout need them gone.
            wait_group_gone(child.id());
        }
        match waited {
            Err(e) => Err(Failure::Io(e.to_string())),
            Ok(None) => Err(Failure::Timeout),
            Ok(Some(status)) if status.success() => {
                String::from_utf8(read_all(&mut stdout).map_err(io)?)
                    .map_err(|e| Failure::Io(e.to_string()))
            }
            Ok(Some(status)) => Err(Failure::Exit {
                code: status.code(),
                stderr: String::from_utf8_lossy(&read_all(&mut stderr).map_err(io)?)
                    .trim()
                    .to_owned(),
            }),
        }
    }

    /// Undoes a `worktree add` of `path` on the new `branch` that was killed part way (its whole process group is
    /// dead by now): removes the worktree's directory, git's record of it and the branch, whichever of them exist.
    /// `create` checked that neither the path nor the branch existed before, so the branch and anything git lists at
    /// `path` are ours. A directory at `path` that git doesn't list is left alone: someone else may have made it since.
    fn undo_add(&self, common: &Path, path: &Path, branch: &BranchName) -> Result<(), String> {
        let cleanup = |op: &str, args: &[&OsStr]| {
            self.git_within(common, args, self.cleanup_timeout)
                .map(drop)
                .map_err(|f| format!("git {op}: {}", f.describe(self.cleanup_timeout)))
        };
        // Double force also removes a worktree still locked as "initializing" by an add killed mid-checkout.
        let remove: [&OsStr; 5] = [
            "worktree".as_ref(),
            "remove".as_ref(),
            "--force".as_ref(),
            "--force".as_ref(),
            path.as_os_str(),
        ];
        let mut left_alone = None;
        if self.registered(common, path)? {
            if let Err(refused) = cleanup("worktree remove", &remove) {
                if path.symlink_metadata().is_err() {
                    return Err(refused);
                }
                // git refused (e.g. the add was killed before it wrote the worktree's `.git` file): delete the
                // directory ourselves, then git drops its record, which it does for a missing directory.
                std::fs::remove_dir_all(path)
                    .map_err(|e| format!("{refused}; can't delete {}: {e}", path.display()))?;
                cleanup("worktree remove", &remove)?;
            }
        } else if path.symlink_metadata().is_ok() {
            left_alone = Some(format!(
                "{} exists but git doesn't list it as a worktree, so it was left alone",
                path.display()
            ));
        }
        // An add killed while it was making the branch leaves the branch's lock behind, which would block deleting
        // the branch and any retry.
        let lock = common.join("refs/heads").join(format!("{branch}.lock"));
        match std::fs::remove_file(&lock) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("can't delete {}: {e}", lock.display()));
            }
            _ => {}
        }
        let full_ref = format!("refs/heads/{branch}");
        let show_ref = ["show-ref", "--verify", "--quiet", &full_ref];
        match self.git_within(common, &show_ref, self.cleanup_timeout) {
            Err(Failure::Exit { .. }) => {}
            Err(f) => {
                return Err(format!(
                    "git show-ref: {}",
                    f.describe(self.cleanup_timeout)
                ));
            }
            Ok(_) => cleanup(
                "update-ref",
                &["update-ref".as_ref(), "-d".as_ref(), full_ref.as_ref()],
            )
            .map_err(|e| format!("can't delete branch {branch}: {e}"))?,
        }
        left_alone.map_or(Ok(()), Err)
    }

    /// Whether git lists a worktree at `path`.
    fn registered(&self, common: &Path, path: &Path) -> Result<bool, String> {
        let list = ["worktree", "list", "--porcelain", "-z"];
        let out = self
            .git_within(common, &list, self.cleanup_timeout)
            .map_err(|f| format!("git worktree list: {}", f.describe(self.cleanup_timeout)))?;
        Ok(out
            .split('\0')
            .filter_map(|field| field.strip_prefix("worktree "))
            .any(|listed| resolved(Path::new(listed)) == resolved(path)))
    }

    /// The absolute common git dir of the checkout containing `dir`.
    fn common_dir(&self, dir: &Path) -> Result<PathBuf, WorktreeError> {
        let out = self
            .git(
                dir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .map_err(|f| f.or_not_a_repo(dir, "rev-parse"))?;
        Ok(PathBuf::from(out.trim_end_matches('\n')))
    }

    fn repo_name(&self, dir: &Path) -> Result<String, WorktreeError> {
        name_of(&self.common_dir(dir)?)
    }

    /// The branch `origin/HEAD` points at (the ref itself may be missing). If there is no `origin/HEAD`, asks origin
    /// for it once with `git remote set-head origin --auto`. Assumes `dir` is in a repo.
    fn default_of(&self, dir: &Path) -> Result<String, WorktreeError> {
        if let Some(branch) = self.origin_head(dir)? {
            return Ok(branch);
        }
        // Best effort: if origin can't be reached, the retry below reports it.
        let _ = self.git_within(
            dir,
            &["remote", "set-head", "origin", "--auto"],
            self.fetch_timeout,
        );
        self.origin_head(dir)?
            .ok_or_else(|| WorktreeError::NoDefaultBranch(dir.to_owned()))
    }

    fn origin_head(&self, dir: &Path) -> Result<Option<String>, WorktreeError> {
        let target = match self.git(
            dir,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        ) {
            Ok(out) => out,
            Err(Failure::Exit { .. }) => return Ok(None),
            Err(f) => return Err(f.into_error("symbolic-ref")),
        };
        Ok(target
            .trim_end_matches('\n')
            .strip_prefix("refs/remotes/origin/")
            .filter(|b| !b.is_empty())
            .map(str::to_owned))
    }

    /// Whether `repo_dir` (`<root>/<repo-name>`) holds a worktree whose common git dir is not `common`.
    fn taken_by_another_repo(&self, repo_dir: &Path, common: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(repo_dir) else {
            return false;
        };
        let ours = std::fs::canonicalize(common).ok();
        entries.flatten().any(|entry| {
            let path = entry.path();
            let args = [
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-common-dir",
            ];
            let Ok(out) = self.git(&path, &args) else {
                return false;
            };
            let mut lines = out.lines().map(std::fs::canonicalize);
            let (Some(Ok(top)), Some(Ok(theirs))) = (lines.next(), lines.next()) else {
                return false;
            };
            std::fs::canonicalize(&path).is_ok_and(|p| p == top) && Some(theirs) != ours
        })
    }

    fn ref_exists(&self, dir: &Path, full_ref: &str) -> Result<bool, WorktreeError> {
        match self.git(dir, &["show-ref", "--verify", "--quiet", full_ref]) {
            Ok(_) => Ok(true),
            Err(Failure::Exit { .. }) => Ok(false),
            Err(f) => Err(f.into_error("show-ref")),
        }
    }

    /// The checkout's uncommitted changes and branch headers, from `status --porcelain=v2 --branch`.
    fn porcelain_status(&self, dir: &Path) -> Result<Porcelain, WorktreeError> {
        let out = self
            .git(
                dir,
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "-z",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
            )
            .map_err(|f| f.into_error("status"))?;
        let mut status = Porcelain::default();
        for field in out.split('\0').filter(|f| !f.is_empty()) {
            if let Some(header) = field.strip_prefix("# ") {
                if let Some(upstream) = header.strip_prefix("branch.upstream ") {
                    status.upstream = Some(upstream.to_owned());
                } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                    status.ahead_of_upstream = ab
                        .split(' ')
                        .find_map(|n| n.strip_prefix('+'))
                        .and_then(|n| n.parse().ok());
                }
            } else {
                status.dirty = true;
            }
        }
        Ok(status)
    }

    /// Checks that `path` is exactly `<root>/<repo>/<name>`, both as written (after resolving `..`) and after
    /// resolving symlinks, and returns the resolved path. `Ok(None)` if it passes as written but doesn't exist.
    fn managed_target(&self, path: &Path) -> Result<Option<PathBuf>, WorktreeError> {
        let outside = || WorktreeError::OutsideManagedRoot(path.to_owned());
        let canon_root = std::fs::canonicalize(&self.root).map_err(|_| outside())?;
        let written_root = normalize_lexically(&self.root).ok_or_else(outside)?;
        let written = normalize_lexically(path)
            .filter(|p| p.is_absolute())
            .ok_or_else(outside)?;
        if managed_parts(&written_root, &written).is_none()
            && managed_parts(&canon_root, &written).is_none()
        {
            return Err(outside());
        }
        let Ok(resolved) = std::fs::canonicalize(&written) else {
            return Ok(None);
        };
        managed_parts(&canon_root, &resolved).ok_or_else(outside)?;
        Ok(Some(resolved))
    }
}

impl Worktrees for GitWorktrees {
    fn default_branch(&self, repo: &RepoRef) -> Result<BranchName, WorktreeError> {
        self.common_dir(repo.as_path())?;
        self.default_of(repo.as_path()).map(BranchName::new)
    }

    fn create(
        &self,
        repo: &RepoRef,
        name: &WorktreeName,
        base: BaseSpec,
    ) -> Result<Created, WorktreeError> {
        let BaseSpec::OriginDefault = base;
        let dir = repo.as_path();
        let common = self.common_dir(dir)?;
        let repo_dir = self.root.join(name_of(&common)?);
        let branch = name.branch();
        let path = repo_dir.join(name.as_str());

        let _guard = self
            .create_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.taken_by_another_repo(&repo_dir, &common) {
            return Err(WorktreeError::RepoNameTaken { path: repo_dir });
        }
        let default = self.default_of(dir)?;
        if self.ref_exists(dir, &format!("refs/heads/{branch}"))? {
            return Err(WorktreeError::BranchExists(branch));
        }
        if path.symlink_metadata().is_ok() {
            return Err(WorktreeError::Other(format!(
                "{} already exists",
                path.display()
            )));
        }
        let fetch = self.git_within(
            dir,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-prune",
                "--no-recurse-submodules",
                "--no-auto-gc",
                "origin",
                &format!("+refs/heads/{default}:refs/remotes/origin/{default}"),
            ],
            self.fetch_timeout,
        );
        let (start, base) = match fetch {
            Ok(_) => (
                format!("refs/remotes/origin/{default}"),
                BaseUsed::Origin {
                    branch: BranchName::new(&default),
                },
            ),
            Err(failure) => {
                let fetch_error = failure.describe(self.fetch_timeout);
                let branch = BranchName::new(&default);
                let stale = format!("refs/remotes/origin/{default}");
                let local = format!("refs/heads/{default}");
                if self.ref_exists(dir, &stale)? {
                    let base = BaseUsed::Stale {
                        branch,
                        fetch_error,
                    };
                    (stale, base)
                } else if self.ref_exists(dir, &local)? {
                    let base = BaseUsed::Local {
                        branch,
                        fetch_error,
                    };
                    (local, base)
                } else {
                    return Err(WorktreeError::FetchFailed(fetch_error));
                }
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorktreeError::Other(format!("can't create {}: {e}", parent.display()))
            })?;
        }
        let args: [&OsStr; 9] = [
            "worktree".as_ref(),
            "add".as_ref(),
            "--quiet".as_ref(),
            "--no-track".as_ref(),
            "--no-guess-remote".as_ref(),
            "-b".as_ref(),
            branch.as_str().as_ref(),
            path.as_os_str(),
            start.as_ref(),
        ];
        match self.git(dir, &args) {
            Ok(_) => {}
            Err(Failure::Timeout) => {
                return Err(match self.undo_add(&common, &path, &branch) {
                    Ok(()) => WorktreeError::Timeout("worktree add".to_owned()),
                    Err(cleanup) => WorktreeError::Other(format!(
                        "git worktree add timed out, and cleaning up failed: {cleanup}. \
                         Remove {} and branch {branch}, and any stale lock git names, by hand",
                        path.display()
                    )),
                });
            }
            Err(other) => return Err(other.into_error("worktree add")),
        }
        Ok(Created {
            path: WorktreePath::new(path),
            branch,
            base,
        })
    }

    fn list_managed(&self, repo: &RepoRef) -> Result<Vec<Worktree>, WorktreeError> {
        let dir = repo.as_path();
        let repo_name = self.repo_name(dir)?;
        let out = self
            .git(dir, &["worktree", "list", "--porcelain", "-z"])
            .map_err(|f| f.into_error("worktree list"))?;
        let Ok(canon_root) = std::fs::canonicalize(&self.root) else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        for record in out.split("\0\0").filter(|r| !r.is_empty()) {
            let mut path = None;
            let mut branch = None;
            for field in record.split('\0') {
                if let Some(p) = field.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p));
                } else if let Some(b) = field.strip_prefix("branch refs/heads/") {
                    branch = Some(BranchName::new(b));
                }
            }
            let Some(path) = path else { continue };
            let resolved = std::fs::canonicalize(&path).unwrap_or(path);
            if let Some((r, n)) = managed_parts(&canon_root, &resolved)
                && r == repo_name
            {
                found.push(Worktree {
                    path: WorktreePath::new(self.root.join(r).join(n)),
                    branch,
                });
            }
        }
        Ok(found)
    }

    fn status(&self, path: &WorktreePath) -> Result<WorktreeStatus, WorktreeError> {
        let dir = path.as_path();
        self.common_dir(dir)?;
        let default = self.default_of(dir)?;
        let origin_default = format!("refs/remotes/origin/{default}");
        if !self.ref_exists(dir, &origin_default)? {
            return Err(WorktreeError::NoDefaultBranch(dir.to_owned()));
        }
        let porcelain = self.porcelain_status(dir)?;
        let range = format!("{origin_default}..HEAD");
        let ahead = self
            .git(dir, &["rev-list", "--count", &range])
            .map_err(|f| f.into_error("rev-list"))?;
        let ahead: u32 = ahead
            .trim()
            .parse()
            .map_err(|_| WorktreeError::Other(format!("unexpected rev-list output {ahead:?}")))?;
        Ok(WorktreeStatus {
            dirty: porcelain.dirty,
            ahead,
            pushed: porcelain.upstream.is_some() && porcelain.ahead_of_upstream == Some(0),
            merged_into_default: ahead == 0,
        })
    }

    fn remove(&self, path: &WorktreePath, force: bool) -> Result<(), WorktreeError> {
        let target = self
            .managed_target(path.as_path())?
            .ok_or_else(|| WorktreeError::NotARepo(path.as_path().to_owned()))?;
        let common = self.common_dir(&target)?;
        let toplevel = self
            .git(&target, &["rev-parse", "--show-toplevel"])
            .map_err(|f| f.or_not_a_repo(&target, "rev-parse"))?;
        let toplevel = std::fs::canonicalize(toplevel.trim_end_matches('\n'))
            .map_err(|e| WorktreeError::Other(e.to_string()))?;
        if toplevel != target {
            return Err(WorktreeError::NotARepo(path.as_path().to_owned()));
        }
        if !force && self.porcelain_status(&target)?.dirty {
            return Err(WorktreeError::Dirty(path.clone()));
        }
        let mut args: Vec<&OsStr> = vec!["worktree".as_ref(), "remove".as_ref()];
        if force {
            args.push("--force".as_ref());
        }
        args.push(target.as_os_str());
        self.git(&common, &args)
            .map_err(|f| f.into_error("worktree remove"))?;
        Ok(())
    }
}

/// `path` with symlinks resolved, as far as it exists: a missing last component is kept as is.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_owned()),
        _ => path.to_owned(),
    }
}

fn name_of(common: &Path) -> Result<String, WorktreeError> {
    repo_name_from_common_dir(common)
        .ok_or_else(|| WorktreeError::Other(format!("can't name the repo at {}", common.display())))
}

/// How a git call went wrong.
#[derive(Debug)]
enum Failure {
    Io(String),
    Timeout,
    Exit { code: Option<i32>, stderr: String },
}

impl Failure {
    fn describe(&self, timeout: Duration) -> String {
        match self {
            Self::Io(e) => format!("can't run git: {e}"),
            Self::Timeout => format!("timed out after {timeout:?}"),
            Self::Exit {
                code: Some(code),
                stderr,
            } => format!("exit {code}: {stderr}"),
            Self::Exit { code: None, stderr } => format!("killed by a signal: {stderr}"),
        }
    }

    fn into_error(self, op: &str) -> WorktreeError {
        match self {
            Self::Timeout => WorktreeError::Timeout(op.to_owned()),
            other => WorktreeError::Other(format!("git {op}: {}", other.describe(Duration::ZERO))),
        }
    }

    /// For calls where git exiting with an error means `dir` is not in a repo (or doesn't exist).
    fn or_not_a_repo(self, dir: &Path, op: &str) -> WorktreeError {
        match self {
            Self::Exit { .. } => WorktreeError::NotARepo(dir.to_owned()),
            other => other.into_error(op),
        }
    }
}

#[derive(Debug, Default)]
struct Porcelain {
    dirty: bool,
    upstream: Option<String>,
    /// `None` when the upstream is configured but its ref is gone.
    ahead_of_upstream: Option<u32>,
}

/// Everything written to `file` so far, from the start.
fn read_all(file: &mut File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscall. The leader is not reaped yet, so the group id still names our group; if the group
        // has no members left it fails with ESRCH, which is fine.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

/// Blocks until child `pid` has exited, leaving it a zombie (`WNOWAIT`) so its pid stays taken.
fn wait_unreaped(pid: u32) {
    let pid = libc::id_t::from(pid);
    loop {
        // SAFETY: `info` is a valid out-pointer; `pid` is our own unreaped child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// How long to wait for the members of a killed process group to finish dying. SIGKILL can't be caught, so this is
/// only a bound for a process stuck in an uninterruptible syscall.
const GROUP_EXIT_WAIT: Duration = Duration::from_secs(10);

/// Blocks until process group `pgid` has no members left, or [`GROUP_EXIT_WAIT`] has passed. Its id can't be reused
/// by a new process while the group has members.
fn wait_group_gone(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    let started = Instant::now();
    // SAFETY: plain syscall; signal 0 only checks that the group exists.
    while unsafe { libc::killpg(pgid, 0) } == 0 && started.elapsed() < GROUP_EXIT_WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs::Permissions;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, native_pty_system};
    use shepherd_core::ports::{
        BaseSpec, BaseUsed, BranchName, RepoRef, WorktreeError, WorktreeName, WorktreePath,
        Worktrees,
    };
    use shepherd_testkit::contracts::worktrees::{
        DEFAULT, FsWorktreesFixture, REPO_NAME, SUBDIR, WorktreesFixture,
    };
    use tempfile::TempDir;

    use super::GitWorktrees;

    /// `<tmp>/seed` (someone else's clone, used to push to origin), `<tmp>/origin.git`, `<tmp>/app` (the clone under
    /// test), `<tmp>/root` (the managed root), `<tmp>/home`. Git never sees the user's config.
    struct GitFixture {
        dir: TempDir,
        port: GitWorktrees,
        counter: AtomicU32,
    }

    impl GitFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let fixture = Self {
                port: port(dir.path(), TEST_TIMEOUT),
                dir,
                counter: AtomicU32::new(0),
            };
            let tmp = fixture.tmp();
            std::fs::create_dir(tmp.join("home")).unwrap();
            std::fs::create_dir(tmp.join("plain")).unwrap();
            fixture.git(
                &tmp,
                &["init", "--quiet", "--bare", "-b", DEFAULT, "origin.git"],
            );
            fixture.git(&tmp, &["init", "--quiet", "-b", DEFAULT, "seed"]);
            let seed = tmp.join("seed");
            std::fs::create_dir(seed.join(SUBDIR)).unwrap();
            std::fs::write(seed.join(SUBDIR).join("lib.txt"), "lib\n").unwrap();
            fixture.git(&seed, &["add", SUBDIR]);
            fixture.commit_in(&seed);
            let origin = tmp.join("origin.git");
            fixture.git(
                &seed,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            fixture.git(&seed, &["push", "--quiet", "origin", DEFAULT]);
            fixture.git(&tmp, &["clone", "--quiet", "origin.git", REPO_NAME]);
            fixture
        }

        fn tmp(&self) -> PathBuf {
            self.dir.path().to_owned()
        }

        fn app(&self) -> PathBuf {
            self.tmp().join(REPO_NAME)
        }

        fn git_output(&self, dir: &Path, args: &[&str]) -> Output {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env_clear()
                .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
                .envs(env(self.dir.path()))
                .output()
                .unwrap()
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            let out = self.git_output(dir, args);
            assert!(
                out.status.success(),
                "git {args:?} in {}: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }

        fn commit_in(&self, dir: &Path) {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            let file = format!("file-{n}.txt");
            std::fs::write(dir.join(&file), format!("{n}\n")).unwrap();
            self.git(dir, &["add", &file]);
            self.git(dir, &["commit", "--quiet", "-m", &format!("commit {n}")]);
        }
    }

    /// The contract runs under heavy parallel load, where a single git call can take minutes. These cases are not
    /// about timeouts (except the hanging fetch, which sets its own), so the limits are out of the way.
    const TEST_TIMEOUT: Duration = Duration::from_secs(900);

    fn port(tmp: &Path, fetch_timeout: Duration) -> GitWorktrees {
        GitWorktrees::new(tmp.join("root"))
            .with_isolated_env(env(tmp))
            .with_git_timeout(TEST_TIMEOUT)
            .with_fetch_timeout(fetch_timeout)
            .with_cleanup_timeout(TEST_TIMEOUT)
    }

    /// Isolates git from the user's machine: no global or system config, a temp `HOME`, a fixed identity.
    fn env(tmp: &Path) -> Vec<(OsString, OsString)> {
        let home = tmp.join("home");
        [
            ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("HOME", home.clone().into()),
            ("XDG_CONFIG_HOME", home.join(".config").into()),
            ("GIT_AUTHOR_NAME", "Shepherd Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@shepherd.invalid".into()),
            ("GIT_COMMITTER_NAME", "Shepherd Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@shepherd.invalid".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect()
    }

    impl WorktreesFixture for GitFixture {
        type Port = GitWorktrees;

        fn port(&self) -> &GitWorktrees {
            &self.port
        }

        fn root(&self) -> PathBuf {
            self.tmp().join("root")
        }

        fn repo(&self) -> RepoRef {
            RepoRef::new(self.app())
        }

        fn not_a_repo(&self) -> RepoRef {
            RepoRef::new(self.tmp().join("plain"))
        }

        fn head(&self, path: &Path) -> String {
            self.git(path, &["rev-parse", "HEAD"])
        }

        fn origin_tip(&self) -> String {
            self.git(&self.tmp().join("origin.git"), &["rev-parse", DEFAULT])
        }

        fn local_default_tip(&self) -> String {
            self.git(
                &self.app(),
                &["rev-parse", &format!("refs/heads/{DEFAULT}")],
            )
        }

        fn advance_origin(&self) {
            let seed = self.tmp().join("seed");
            self.commit_in(&seed);
            self.git(&seed, &["push", "--quiet", "origin", DEFAULT]);
        }

        fn commit(&self, path: &WorktreePath) {
            self.commit_in(path.as_path());
        }

        fn make_dirty(&self, path: &WorktreePath) {
            std::fs::write(path.as_path().join("untracked.txt"), "wip\n").unwrap();
        }

        fn push(&self, path: &WorktreePath) {
            self.git(path.as_path(), &["push", "--quiet", "-u", "origin", "HEAD"]);
        }

        fn land_on_origin_default(&self, path: &WorktreePath) {
            let refspec = format!("HEAD:{DEFAULT}");
            self.git(path.as_path(), &["push", "--quiet", "origin", &refspec]);
            self.git(&self.app(), &["fetch", "--quiet", "origin"]);
        }

        fn go_offline(&self) {
            let nowhere = self.tmp().join("nowhere.git");
            self.git(
                &self.app(),
                &["remote", "set-url", "origin", nowhere.to_str().unwrap()],
            );
        }

        /// An ssh remote whose "ssh" is a long sleep, so the fetch hangs without touching the network.
        fn hang_origin(&self, timeout: Duration) -> GitWorktrees {
            let app = self.app();
            self.git(&app, &["config", "core.sshCommand", "sh -c 'sleep 900' --"]);
            self.git(&app, &["config", "ssh.variant", "simple"]);
            self.git(
                &app,
                &["remote", "set-url", "origin", "ssh://hang.invalid/x.git"],
            );
            port(self.dir.path(), timeout)
        }

        fn delete_local_default(&self) {
            let app = self.app();
            self.git(&app, &["checkout", "--quiet", "--detach"]);
            self.git(&app, &["branch", "--quiet", "-D", DEFAULT]);
        }

        fn fetch(&self) {
            self.git(&self.app(), &["fetch", "--quiet", "origin"]);
        }

        fn delete_remote_tracking_default(&self) {
            let tracking = format!("refs/remotes/origin/{DEFAULT}");
            self.git(&self.app(), &["update-ref", "-d", &tracking]);
        }

        fn forget_origin_head(&self) {
            self.git(&self.app(), &["remote", "set-head", "origin", "--delete"]);
        }

        fn same_named_repo(&self) -> RepoRef {
            let tmp = self.tmp();
            std::fs::create_dir(tmp.join("other")).unwrap();
            let origin = tmp.join("origin.git");
            self.git(
                &tmp.join("other"),
                &["clone", "--quiet", origin.to_str().unwrap(), REPO_NAME],
            );
            RepoRef::new(tmp.join("other").join(REPO_NAME))
        }
    }

    impl FsWorktreesFixture for GitFixture {
        fn unmanaged_worktree(&self) -> WorktreePath {
            let path = self.tmp().join("outside").join("wt");
            let base = format!("origin/{DEFAULT}");
            self.git(
                &self.app(),
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "outside-wt",
                    path.to_str().unwrap(),
                    &base,
                ],
            );
            WorktreePath::new(path)
        }

        fn symlink(&self, link: &Path, target: &Path) {
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, link).unwrap();
        }
    }

    shepherd_testkit::worktrees_contract!(fs: GitFixture::new());

    // ---------- adapter only ----------

    fn name(s: &str) -> WorktreeName {
        WorktreeName::new(s).unwrap()
    }

    fn create(f: &GitFixture, n: &str) -> WorktreePath {
        f.port
            .create(&f.repo(), &name(n), BaseSpec::OriginDefault)
            .unwrap()
            .path
    }

    /// Installs a hook in the clone (hooks are shared by its worktrees).
    fn hook(f: &GitFixture, hook: &str, script: &str) {
        let path = f.app().join(".git").join("hooks").join(hook);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
    }

    /// Set in a re-run of this test binary that should run the child half of a test.
    const CHILD_VAR: &str = "SHEPHERD_WORKTREES_TEST_CHILD";

    fn in_child() -> bool {
        std::env::var_os(CHILD_VAR).is_some()
    }

    /// Arguments that re-run only the test `name` of this binary.
    fn rerun_args(name: &str) -> [String; 4] {
        [
            "--exact".into(),
            format!("worktrees::tests::{name}"),
            "--test-threads=1".into(),
            "--nocapture".into(),
        ]
    }

    /// `PATH`, the isolation variables for `tmp`, and [`CHILD_VAR`].
    fn child_env(tmp: &Path) -> Vec<(OsString, OsString)> {
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let mut vars: Vec<_> = std::env::vars_os().filter(|(k, _)| k == "PATH").collect();
        vars.extend(env(tmp));
        vars.push((CHILD_VAR.into(), "1".into()));
        vars
    }

    /// `status.showUntrackedFiles=no` would make untracked files invisible, and `git worktree remove` would then
    /// delete them.
    #[test]
    fn hidden_untracked_files_still_count_as_dirty() {
        let f = GitFixture::new();
        f.git(&f.app(), &["config", "status.showUntrackedFiles", "no"]);
        let path = create(&f, "messy");
        f.make_dirty(&path);
        assert!(f.port.status(&path).unwrap().dirty);
        assert_eq!(
            f.port.remove(&path, false),
            Err(WorktreeError::Dirty(path.clone()))
        );
        assert!(path.as_path().join("untracked.txt").exists());
    }

    #[test]
    fn auto_setup_merge_always_still_gives_no_upstream() {
        let f = GitFixture::new();
        f.git(&f.app(), &["config", "branch.autoSetupMerge", "always"]);
        let path = create(&f, "fresh");
        assert!(!f.port.status(&path).unwrap().pushed);
        let upstream = f.git_output(&f.app(), &["config", "--get", "branch.fresh.merge"]);
        assert!(!upstream.status.success(), "{upstream:?}");
    }

    #[test]
    fn a_custom_fetch_refspec_still_fetches_the_default_branch() {
        let f = GitFixture::new();
        let refspec = "+refs/heads/nothing:refs/remotes/origin/nothing";
        f.git(&f.app(), &["config", "remote.origin.fetch", refspec]);
        f.advance_origin();
        let created = f
            .port
            .create(&f.repo(), &name("fresh"), BaseSpec::OriginDefault)
            .unwrap();
        assert_eq!(
            created.base,
            BaseUsed::Origin {
                branch: BranchName::new(DEFAULT)
            }
        );
        assert_eq!(f.head(created.path.as_path()), f.origin_tip());
    }

    #[test]
    fn a_slow_git_call_is_a_typed_timeout() {
        let f = GitFixture::new();
        blocking_hook(&f, "");
        let started = Instant::now();
        let result = slow_port(&f).create(&f.repo(), &name("slow"), BaseSpec::OriginDefault);
        assert_eq!(result, Err(WorktreeError::Timeout("worktree add".into())));
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "took {:?}",
            started.elapsed()
        );
    }

    /// The `worktree add` bound in the timed-out-create tests: comfortably more than a checkout takes under load. The
    /// hook never returns, so only this timeout firing is timing-dependent.
    const ADD_TIMEOUT: Duration = Duration::from_secs(30);

    /// A port whose `worktree add` (and every other non-fetch call) times out after [`ADD_TIMEOUT`]. Cleanup keeps
    /// the generous test timeout.
    fn slow_port(f: &GitFixture) -> GitWorktrees {
        port(f.dir.path(), TEST_TIMEOUT).with_git_timeout(ADD_TIMEOUT)
    }

    /// Installs a `post-checkout` hook that runs `setup`, then blocks forever reading a FIFO nobody writes.
    /// `post-checkout` runs after git has registered the worktree and made its branch.
    fn blocking_hook(f: &GitFixture, setup: &str) {
        let fifo = f.tmp().join("never.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        hook(
            f,
            "post-checkout",
            &format!("{setup}\ncat '{}' > /dev/null", fifo.display()),
        );
    }

    fn branch_exists(f: &GitFixture, branch: &str) -> bool {
        let reference = format!("refs/heads/{branch}");
        f.git_output(&f.app(), &["show-ref", "--verify", "--quiet", &reference])
            .status
            .success()
    }

    fn worktree_path(f: &GitFixture, n: &str) -> PathBuf {
        f.root().join(REPO_NAME).join(n)
    }

    /// No directory, no record in git (managed or not) and no branch for `n`.
    fn assert_nothing_left(f: &GitFixture, n: &str) {
        assert!(!worktree_path(f, n).exists());
        let listed = f.git(&f.app(), &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains(n), "{listed}");
        let admin = f.app().join(".git").join("worktrees").join(n);
        assert!(!admin.exists(), "{}", admin.display());
        assert!(!branch_exists(f, n), "branch {n}");
    }

    fn retry_succeeds(f: &GitFixture, n: &str) {
        hook(f, "post-checkout", "exit 0");
        let created = f
            .port
            .create(&f.repo(), &name(n), BaseSpec::OriginDefault)
            .unwrap();
        assert!(created.path.as_path().is_dir());
        assert!(branch_exists(f, n));
    }

    #[test]
    fn a_timed_out_create_leaves_nothing_behind_and_can_be_retried() {
        let f = GitFixture::new();
        blocking_hook(&f, "");
        let result = slow_port(&f).create(&f.repo(), &name("slow"), BaseSpec::OriginDefault);
        assert_eq!(result, Err(WorktreeError::Timeout("worktree add".into())));
        assert_nothing_left(&f, "slow");
        retry_succeeds(&f, "slow");
    }

    /// What an add killed while it makes the branch leaves behind.
    #[test]
    fn a_timed_out_create_removes_the_new_branchs_stale_lock() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/refs/heads/locked.lock");
        blocking_hook(&f, &format!("touch '{}'", lock.display()));
        let result = slow_port(&f).create(&f.repo(), &name("locked"), BaseSpec::OriginDefault);
        assert_eq!(result, Err(WorktreeError::Timeout("worktree add".into())));
        assert!(!lock.exists());
        assert_nothing_left(&f, "locked");
        retry_succeeds(&f, "locked");
    }

    /// Without its `.git` file (as when the add is killed before writing it) git refuses to remove the worktree.
    #[test]
    fn a_timed_out_create_deletes_a_worktree_git_refuses_to_remove() {
        let f = GitFixture::new();
        let keep = create(&f, "keep");
        let keep_head = f.head(keep.as_path());
        let path = worktree_path(&f, "broken");
        blocking_hook(&f, &format!("rm '{}'", path.join(".git").display()));
        let result = slow_port(&f).create(&f.repo(), &name("broken"), BaseSpec::OriginDefault);
        assert_eq!(result, Err(WorktreeError::Timeout("worktree add".into())));
        assert_nothing_left(&f, "broken");

        assert_eq!(f.head(keep.as_path()), keep_head);
        let managed = f.port.list_managed(&f.repo()).unwrap();
        assert_eq!(managed.len(), 1, "{managed:?}");
        assert_eq!(managed[0].path, keep);
        assert!(f.app().join(SUBDIR).join("lib.txt").exists());
        retry_succeeds(&f, "broken");
    }

    /// Another process replaced the half-made worktree with its own directory, which git doesn't know.
    #[test]
    fn a_timed_out_create_leaves_a_directory_git_does_not_list_alone() {
        let f = GitFixture::new();
        let path = worktree_path(&f, "taken");
        let admin = f.app().join(".git").join("worktrees").join("taken");
        let theirs = path.join("theirs.txt");
        blocking_hook(
            &f,
            &format!(
                "rm -rf '{admin}' '{path}'\nmkdir '{path}'\ntouch '{theirs}'",
                admin = admin.display(),
                path = path.display(),
                theirs = theirs.display(),
            ),
        );
        let result = slow_port(&f).create(&f.repo(), &name("taken"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("left alone"), "{message}");
        assert!(theirs.exists());
        assert!(!branch_exists(&f, "taken"));
    }

    /// `packed-refs.lock` (not ours to remove) makes deleting the new branch fail.
    #[test]
    fn a_timed_out_create_whose_cleanup_fails_says_so() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/packed-refs.lock");
        blocking_hook(&f, &format!("touch '{}'", lock.display()));
        let result = slow_port(&f).create(&f.repo(), &name("stuck"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("can't delete branch stuck"), "{message}");
    }

    /// The background `sleep` keeps git's stdout and stderr open long after git exits.
    #[test]
    fn a_hook_that_leaves_a_background_process_does_not_hold_the_call() {
        let f = GitFixture::new();
        let pid_file = f.tmp().join("background.pid");
        hook(
            &f,
            "post-checkout",
            &format!("sleep 900 &\necho $! > '{}'", pid_file.display()),
        );
        let started = Instant::now();
        let result = f
            .port
            .create(&f.repo(), &name("bg"), BaseSpec::OriginDefault);
        let elapsed = started.elapsed();
        if let Ok(pid) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
        {
            // SAFETY: plain syscall on the pid the hook reported.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(result.is_ok(), "{result:?}");
        assert!(elapsed < Duration::from_secs(300), "took {elapsed:?}");
    }

    /// Where the injected hook in [`config_injected_through_the_environment_is_ignored`] leaves its mark.
    const MARKER_VAR: &str = "SHEPHERD_WORKTREES_TEST_MARKER";

    /// Re-runs itself with `core.hooksPath` injected through the environment (both ways git reads it from there),
    /// pointing at a `post-checkout` hook that leaves a marker, and a port that inherits that environment.
    #[test]
    fn config_injected_through_the_environment_is_ignored() {
        if in_child() {
            let marker = PathBuf::from(std::env::var_os(MARKER_VAR).unwrap());
            let f = GitFixture::new();
            let port = GitWorktrees::new(f.root())
                .with_git_timeout(TEST_TIMEOUT)
                .with_fetch_timeout(TEST_TIMEOUT);
            port.create(&f.repo(), &name("x"), BaseSpec::OriginDefault)
                .unwrap();
            assert!(!marker.exists(), "the injected hooksPath was used");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let hooks = tmp.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        let marker = tmp.path().join("injected");
        let hook = hooks.join("post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, Permissions::from_mode(0o755)).unwrap();
        let hooks = hooks.to_str().unwrap();
        let injections: [Vec<(&str, String)>; 2] = [
            vec![
                ("GIT_CONFIG_COUNT", "1".into()),
                ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
                ("GIT_CONFIG_VALUE_0", hooks.into()),
            ],
            vec![(
                "GIT_CONFIG_PARAMETERS",
                format!("'core.hookspath'='{hooks}'"),
            )],
        ];
        for injection in injections {
            let out = Command::new(std::env::current_exe().unwrap())
                .args(rerun_args(
                    "config_injected_through_the_environment_is_ignored",
                ))
                .env_clear()
                .envs(child_env(tmp.path()))
                .env(MARKER_VAR, &marker)
                .envs(injection.iter().map(|(k, v)| (k, v)))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{injection:?}\n{stdout}\n{stderr}");
            assert!(stdout.contains("1 passed"), "{stdout}");
        }
    }

    /// Re-runs itself in a pty, so it has a controlling terminal that ssh could prompt on, then checks that the "ssh"
    /// git runs can't open `/dev/tty`.
    #[test]
    fn ssh_runs_without_a_controlling_terminal() {
        if in_child() {
            assert!(
                std::fs::File::open("/dev/tty").is_ok(),
                "the re-run should have a controlling terminal"
            );
            let f = GitFixture::new();
            let report = f.tmp().join("tty-report");
            let ssh = format!(
                "sh -c 'if (exec 3</dev/tty) 2>/dev/null; then echo tty; else echo none; fi > \"{}\"; exit 1' --",
                report.display()
            );
            let app = f.app();
            f.git(&app, &["config", "core.sshCommand", &ssh]);
            f.git(&app, &["config", "ssh.variant", "simple"]);
            f.git(
                &app,
                &["remote", "set-url", "origin", "ssh://tty.invalid/x.git"],
            );
            let created = f
                .port
                .create(&f.repo(), &name("no-tty"), BaseSpec::OriginDefault)
                .unwrap();
            assert!(
                matches!(created.base, BaseUsed::Stale { .. }),
                "{:?}",
                created.base
            );
            assert_eq!(std::fs::read_to_string(&report).unwrap().trim(), "none");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let pty = native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(std::env::current_exe().unwrap());
        cmd.args(rerun_args("ssh_runs_without_a_controlling_terminal"));
        cmd.cwd(tmp.path());
        cmd.env_clear();
        for (k, v) in child_env(tmp.path()) {
            cmd.env(k, v);
        }
        let mut child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        });
        let status = child.wait().unwrap();
        drop(pty.master);
        let output = String::from_utf8_lossy(&output.join().unwrap()).into_owned();
        assert!(status.success(), "{output}");
        assert!(output.contains("1 passed"), "{output}");
    }

    #[test]
    fn accepted_worktree_names_are_valid_branch_names() {
        let f = GitFixture::new();
        for n in [
            "fix-login",
            "a",
            "T20.worktrees",
            "s_01abc",
            "head",
            "v1.2.3-rc_1",
            "0lead",
            "x-",
            "a_",
        ] {
            assert!(WorktreeName::new(n).is_ok(), "{n}");
            f.git(&f.tmp(), &["check-ref-format", "--branch", n]);
        }
    }
}
