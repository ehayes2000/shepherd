//! `Worktrees` over the `git` CLI. Every call runs `git -C <dir> ...` and parses only porcelain or plumbing output.
//!
//! # Never waiting on a person
//!
//! Each git process starts a new session (`setsid`), so it has no controlling terminal and neither do its children:
//! ssh can't open `/dev/tty` to ask for a passphrase or host key. Stdin is closed, and the environment turns off the
//! prompts that don't need a terminal: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` (Git Credential Manager) and
//! `SSH_ASKPASS_REQUIRE=never`. A process that runs past its timeout is killed with its whole process group, hooks and
//! ssh included. `worktree add` has no timeout (a big checkout, with git-lfs, takes as long as it takes); it is killed
//! the same way only by [`Worktrees::cancel_create`]. Output goes to temp files rather than pipes, so a background
//! process left behind by a hook can't hold a call open. A `worktree add` killed by a cancel is undone: its worktree
//! and new branch are removed (see [`Worktrees::create`]). So is one that fails any other way, e.g. a `post-checkout`
//! hook that exits non-zero.
//!
//! Each create runs on its caller's thread, so creates for different sessions run at the same time. Their checks, fetch
//! and branch creation take turns (concurrent fetches into one repo race on ref locks); their checkouts don't.
//!
//! The new branch is made atomically with `update-ref` before the add, which fails if the branch exists, so cleanup
//! never touches a branch `create` didn't make, and deletes it only while it still points where `create` put it.
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
//! - `worktree add`: `--no-guess-remote` (beats `worktree.guessRemote`). The branch it checks out is made with
//!   `update-ref`, which never sets an upstream (so `branch.autoSetupMerge` doesn't apply).
//! - `update-ref` (making and deleting the new branch) and `worktree add`: `-c core.filesRefLockTimeout=0`, so they
//!   never wait on another process's lock of the new branch, and a lock left after one of them is killed is its own.
//! - Config injected through the environment (`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and its
//!   `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`) is removed, as are variables that point git at another repo
//!   (`GIT_DIR`, ...).
//!
//! Left to the user on purpose: hooks (`core.hooksPath`, `post-checkout` on `worktree add`), ssh and credential
//! settings. Nothing here pushes, so `push.default` doesn't matter.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shepherd_core::ports::worktrees::{
    managed_parts, normalize_lexically, repo_name_from_common_dir,
};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, Created, RepoRef, Worktree, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
};

/// For every git call except fetch and `worktree add`: short metadata calls (rev-parse, show-ref, update-ref, worktree
/// list, ...), so generous. `worktree add` has no time cap: it checks out files (a big repo's checkout with git-lfs
/// smudging can take many minutes) and runs the user's hooks; [`Worktrees::cancel_create`] stops it.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// For the calls that lock the new branch (`update-ref`, `worktree add`, deleting it in cleanup): fail at once on a
/// lock that is already taken instead of waiting (`core.filesRefLockTimeout`), so a lock left after such a call is
/// killed is always its own, and cleanup doesn't hang on someone else's.
const NO_REF_LOCK_WAIT: &str = "core.filesRefLockTimeout=0";

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
    /// For the git calls that undo a failed or cancelled `worktree add`. Separate from `git_timeout`, so cleanup still
    /// has room when that timeout was set short.
    cleanup_timeout: Duration,
    /// Serializes the creates' checks, fetch and branch creation: concurrent fetches into one repo race on ref locks.
    /// Released before `worktree add`, so checkouts run at the same time.
    create_lock: Mutex<()>,
    /// Creates running, and creates cancelled before they started, for [`Worktrees::cancel_create`].
    creates: Mutex<HashMap<CreateKey, InFlight>>,
}

/// A create's repo (as the caller named it) and worktree name: what [`Worktrees::cancel_create`] names.
type CreateKey = (PathBuf, String);

/// A create [`GitWorktrees::creates`] knows of.
#[derive(Debug, Default)]
struct InFlight {
    /// `false`: cancelled before it started, so the next create of it is cancelled.
    running: bool,
    cancelled: bool,
    /// The process group of the git call it is waiting on, while that git is unreaped (so the id can't be reused).
    pgid: Option<u32>,
}

/// A running create, registered in [`GitWorktrees::creates`] until it is dropped.
struct CreateCall<'a> {
    creates: &'a Mutex<HashMap<CreateKey, InFlight>>,
    key: CreateKey,
}

impl CreateCall<'_> {
    fn cancelled(&self) -> bool {
        lock(self.creates)
            .get(&self.key)
            .is_some_and(|c| c.cancelled)
    }

    /// Records that git runs as process group `pgid`. Whether the create is already cancelled, so the caller kills it.
    fn spawned(&self, pgid: u32) -> bool {
        let mut creates = lock(self.creates);
        let Some(call) = creates.get_mut(&self.key) else {
            return false;
        };
        call.pgid = Some(pgid);
        call.cancelled
    }

    /// Forgets git's process group: called once git has exited, before it is reaped.
    fn exited(&self) {
        if let Some(call) = lock(self.creates).get_mut(&self.key) {
            call.pgid = None;
        }
    }
}

impl Drop for CreateCall<'_> {
    fn drop(&mut self) {
        lock(self.creates).remove(&self.key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
            creates: Mutex::default(),
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

    /// The timeout for every other git call but `worktree add`, which has none. One that runs past it is
    /// [`WorktreeError::Timeout`].
    pub fn with_git_timeout(mut self, timeout: Duration) -> Self {
        self.git_timeout = timeout;
        self
    }

    /// The timeout for the git calls that clean up after a failed or cancelled `worktree add`.
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
        self.run(dir, args, Some(timeout), None)
    }

    /// Runs git in `dir`, killing it (its whole process group) once `timeout` passes, if there is one, or once `call`
    /// is cancelled, if it belongs to a create. Either way the group is gone when this returns.
    fn run<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Option<Duration>,
        call: Option<&CreateCall>,
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
        let pid = child.id();
        // Until git is reaped its pid (the group id) can't be reused by another process, so git is waited for without
        // reaping it, and its group killed if need be, before it is reaped.
        let (exited_tx, exited) = mpsc::channel();
        let waiter = std::thread::Builder::new()
            .name("git-wait".into())
            .spawn(move || exited_tx.send(wait_unreaped(pid)));
        if let Err(e) = waiter {
            kill_group(pid);
            let _ = child.wait();
            return Err(io(e));
        }
        // A cancel kills the group from here on (or now, if it came first), until git has exited.
        if call.is_some_and(|c| c.spawned(pid)) {
            kill_group(pid);
        }
        let waited_for = match timeout {
            Some(timeout) => exited.recv_timeout(timeout),
            None => exited.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let (timed_out, kill) = match waited_for {
            // git was killed (by a cancel, or e.g. by its own hook), so nothing stopped its children: kill them as for
            // a timeout.
            Ok(how) => (false, how.is_ok_and(killed_by_signal)),
            Err(RecvTimeoutError::Disconnected) => (false, false),
            Err(RecvTimeoutError::Timeout) => {
                kill_group(pid);
                // Then again once git is dead, for a hook it forked while the first kill was being delivered.
                let _ = exited.recv();
                (true, true)
            }
        };
        // git has exited, and is still unreaped: a cancel from now on mustn't kill its group id.
        if let Some(call) = call {
            call.exited();
        }
        let cancelled = kill && call.is_some_and(CreateCall::cancelled);
        if kill {
            kill_group(pid);
        }
        let waited = child.wait();
        if kill {
            // git's own children (e.g. the `git reset` that `worktree add` runs) may still be dying, and a syscall in
            // flight, such as renaming a ref lock into place, completes first. Callers that clean up after a timeout
            // need them gone.
            wait_group_gone(pid);
        }
        match waited {
            Err(e) => Err(Failure::Io(e.to_string())),
            Ok(_) if timed_out => Err(Failure::Timeout),
            Ok(_) if cancelled => Err(Failure::Cancelled),
            Ok(status) if status.success() => String::from_utf8(read_all(&mut stdout).map_err(io)?)
                .map_err(|e| Failure::Io(e.to_string())),
            Ok(status) => Err(Failure::Exit {
                code: status.code(),
                stderr: String::from_utf8_lossy(&read_all(&mut stderr).map_err(io)?)
                    .trim()
                    .to_owned(),
            }),
        }
    }

    /// Makes `branch` at `oid`, atomically: if the branch exists, e.g. made by another process since `create` checked
    /// (say, during the fetch), this fails without touching it and there is nothing to clean up.
    /// Registers a create of `name` in `repo` for [`Worktrees::cancel_create`], unless one was cancelled before it
    /// started ([`WorktreeError::Cancelled`], consuming that cancel) or one is already running.
    fn start_create(
        &self,
        repo: &RepoRef,
        name: &WorktreeName,
    ) -> Result<CreateCall<'_>, WorktreeError> {
        let key = (repo.as_path().to_owned(), name.as_str().to_owned());
        let mut creates = lock(&self.creates);
        match creates.get(&key) {
            Some(InFlight { running: false, .. }) => {
                creates.remove(&key);
                return Err(WorktreeError::Cancelled);
            }
            Some(_) => {
                return Err(WorktreeError::Other(format!(
                    "{name} is already being created"
                )));
            }
            None => {}
        }
        let running = InFlight {
            running: true,
            ..InFlight::default()
        };
        creates.insert(key.clone(), running);
        Ok(CreateCall {
            creates: &self.creates,
            key,
        })
    }

    fn create_branch(
        &self,
        dir: &Path,
        common: &Path,
        branch: &BranchName,
        oid: &str,
        call: &CreateCall,
    ) -> Result<(), WorktreeError> {
        let full_ref = format!("refs/heads/{branch}");
        // The empty old value means "must not exist yet".
        let args = ["-c", NO_REF_LOCK_WAIT, "update-ref", &full_ref, oid, ""];
        match self.run(dir, &args, Some(self.git_timeout), Some(call)) {
            Ok(_) => Ok(()),
            Err(Failure::Exit { code: Some(_), .. }) if self.ref_exists(dir, &full_ref)? => {
                Err(WorktreeError::BranchExists(branch.clone()))
            }
            // Exited on its own, so it rolled back and released its lock.
            Err(failure @ (Failure::Exit { code: Some(_), .. } | Failure::Io(_))) => {
                let failure = failure.describe(self.git_timeout);
                Err(WorktreeError::Other(format!(
                    "can't create branch {branch}: git update-ref: {failure}"
                )))
            }
            // Killed (timed out, cancelled, or by a signal), so it may have died holding the branch's lock, which would
            // block any retry. It never waits for a lock (`NO_REF_LOCK_WAIT`), so a lock there now is one it took, not
            // another process's it was waiting on.
            Err(failure) => {
                let lock = common.join("refs/heads").join(format!("{branch}.lock"));
                match std::fs::remove_file(&lock) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        Err(WorktreeError::Other(format!(
                            "git update-ref: {}, and can't delete {}: {e}",
                            failure.describe(self.git_timeout),
                            lock.display()
                        )))
                    }
                    _ => Err(failure.into_error("update-ref")),
                }
            }
        }
    }

    /// Undoes a failed `worktree add` of `path` on the branch `create` just made (the add's whole process group is
    /// dead by now): removes the worktree's directory, git's record of it and the branch, whichever of them exist and
    /// `ours` allows. Anything git lists at `path` is ours unless it was listed before the add. A directory at `path`
    /// that git doesn't list is left alone, since someone else may have made it since, and so is the branch if it no
    /// longer points where `create` put it; the `Ok` value then says so.
    fn undo_add(
        &self,
        common: &Path,
        path: &Path,
        made: NewBranch,
        ours: Leftovers,
    ) -> Result<Vec<String>, String> {
        let branch = made.name;
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
        let mut left_alone = Vec::new();
        if !ours.worktree {
            // Listed before the add: not ours.
        } else if self.registered(common, path)? {
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
            left_alone.push(format!(
                "{} wasn't created by shepherd, so it was left alone.",
                path.display()
            ));
        }
        // An add killed while it was making the branch leaves the branch's lock behind, which would block deleting
        // the branch and any retry.
        let lock = common.join("refs/heads").join(format!("{branch}.lock"));
        match ours.branch_lock.then(|| std::fs::remove_file(&lock)) {
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("can't delete {}: {e}", lock.display()));
            }
            _ => {}
        }
        // Deleted only if it still points where `create` put it.
        let full_ref = format!("refs/heads/{branch}");
        // Without waiting on a lock someone else holds: that fails the delete, and the user is told.
        let delete: [&OsStr; 6] = [
            "-c".as_ref(),
            NO_REF_LOCK_WAIT.as_ref(),
            "update-ref".as_ref(),
            "-d".as_ref(),
            full_ref.as_ref(),
            made.oid.as_ref(),
        ];
        let Err(refused) = cleanup("update-ref", &delete) else {
            return Ok(left_alone);
        };
        let show_ref = ["show-ref", "--verify", "--quiet", &full_ref];
        match self.git_within(common, &show_ref, self.cleanup_timeout) {
            // Someone else deleted it.
            Err(Failure::Exit { .. }) => return Ok(left_alone),
            Err(f) => {
                return Err(format!(
                    "git show-ref: {}",
                    f.describe(self.cleanup_timeout)
                ));
            }
            Ok(_) => {}
        }
        let points_at = format!("--points-at={}", made.oid);
        let ours_still = ["for-each-ref", "--format=%(refname)", &points_at, &full_ref];
        let listed = self
            .git_within(common, &ours_still, self.cleanup_timeout)
            .map_err(|f| format!("git for-each-ref: {}", f.describe(self.cleanup_timeout)))?;
        if listed.trim().is_empty() {
            left_alone.push(format!(
                "branch {branch} was moved after shepherd created it, so it was left alone."
            ));
            return Ok(left_alone);
        }
        Err(format!("can't delete branch {branch}: {refused}"))
    }

    /// Undoes a `worktree add` that `what` (e.g. "timed out") with [`Self::undo_add`] and returns `error`, or, if
    /// cleaning up fails too, an error saying so and what to remove by hand, or, if a directory was left alone, an
    /// error saying that.
    fn undo_failed_add(
        &self,
        common: &Path,
        path: &Path,
        made: NewBranch,
        ours: Leftovers,
        what: &str,
        error: WorktreeError,
    ) -> WorktreeError {
        let branch = made.name;
        match self.undo_add(common, path, made, ours) {
            Ok(left_alone) if left_alone.is_empty() => error,
            Ok(left_alone) => {
                WorktreeError::Other(format!("git worktree add {what}. {}", left_alone.join(" ")))
            }
            Err(cleanup) => WorktreeError::Other(format!(
                "git worktree add {what}, and cleaning up failed: {cleanup}. \
                 Remove {} and branch {branch}, and any stale lock git names, by hand",
                path.display()
            )),
        }
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

    /// Where `path` is: its checkout's top level, git dir and common git dir, all with symlinks resolved.
    fn checkout_of(&self, path: &Path) -> Result<Placement, WorktreeError> {
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ];
        let out = self
            .git(path, &args)
            .map_err(|f| f.or_not_a_repo(path, "rev-parse"))?;
        // Git prints resolved paths already; resolving again keeps the comparisons in `repo_root` from relying on it.
        let canonical = |line: &str| {
            std::fs::canonicalize(line).map_err(|e| WorktreeError::Other(format!("{line}: {e}")))
        };
        let mut lines = out.lines();
        let (Some(top), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next())
        else {
            return Err(WorktreeError::Other(format!(
                "unexpected rev-parse output {out:?}"
            )));
        };
        Ok(Placement {
            top: canonical(top)?,
            git_dir: canonical(git_dir)?,
            common: canonical(common)?,
        })
    }

    /// The main checkout of the repo the linked worktree `place` belongs to, if it has one: see
    /// [`Worktrees::repo_root`].
    fn main_checkout(&self, place: &Placement) -> Result<Option<PathBuf>, WorktreeError> {
        let out = self
            .git(&place.top, &["worktree", "list", "--porcelain", "-z"])
            .map_err(|f| f.into_error("worktree list"))?;
        // NUL-terminated fields; an empty field ends an entry.
        let first: Vec<&str> = out.split('\0').take_while(|f| !f.is_empty()).collect();
        if first.contains(&"bare") {
            return Ok(None);
        }
        let Some(listed) = first.iter().find_map(|f| f.strip_prefix("worktree ")) else {
            return Ok(None);
        };
        let Ok(main) = self.checkout_of(Path::new(listed)) else {
            return Ok(None);
        };
        // The git-dir and same-repo checks are defensive: git derives the entry from the common dir, so a checkout
        // found there always uses that dir as both its git dir and its common dir.
        let is_main = main.git_dir == main.common
            && main.common == place.common
            && std::fs::canonicalize(listed).is_ok_and(|p| p == main.top);
        Ok(is_main.then_some(main.top))
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
    /// A checkout whose git dir is the common dir is a main checkout (a submodule's is, too). A linked worktree's main
    /// checkout is the first entry of `git worktree list`, unless that entry says the repo is bare, or it isn't a main
    /// checkout of the same repo (e.g. it was deleted); then the worktree names itself.
    ///
    /// Known gap: git derives that entry from the common dir's location. For a linked worktree of a repo whose git dir
    /// lives apart from its main checkout (`git init --separate-git-dir <store>/.git`), git can't know where the main
    /// checkout is and lists `<store>`, which looks like a checkout of the repo, so `<store>` is returned.
    fn repo_root(&self, path: &Path) -> Result<PathBuf, WorktreeError> {
        let place = self.checkout_of(path)?;
        if place.git_dir != place.common
            && let Some(main) = self.main_checkout(&place)?
        {
            return Ok(main);
        }
        Ok(place.top)
    }

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
        let call = self.start_create(repo, name)?;
        let stop_if_cancelled = || match call.cancelled() {
            true => Err(WorktreeError::Cancelled),
            false => Ok(()),
        };
        let dir = repo.as_path();
        let common = self.common_dir(dir)?;
        let repo_dir = self.root.join(name_of(&common)?);
        let branch = name.branch();
        let path = repo_dir.join(name.as_str());

        let guard = lock(&self.create_lock);
        stop_if_cancelled()?;
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
        let fetch = self.run(
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
            Some(self.fetch_timeout),
            Some(&call),
        );
        let (start, base) = match fetch {
            Ok(_) => (
                format!("refs/remotes/origin/{default}"),
                BaseUsed::Origin {
                    branch: BranchName::new(&default),
                },
            ),
            Err(Failure::Cancelled) => return Err(WorktreeError::Cancelled),
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
        let base_oid = self
            .git(
                dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{start}^{{commit}}"),
                ],
            )
            .map_err(|f| f.into_error("rev-parse"))?;
        let base_oid = base_oid.trim_end_matches('\n');
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorktreeError::Other(format!("can't create {}: {e}", parent.display()))
            })?;
        }
        // A stale record of a missing directory at `path` makes the add fail, and isn't ours to remove.
        let registered_before = self
            .registered(&common, &path)
            .map_err(WorktreeError::Other)?;
        stop_if_cancelled()?;
        self.create_branch(dir, &common, &branch, base_oid, &call)?;
        // The checkout runs alongside other creates' (its worktree and branch are its own), with no time cap.
        drop(guard);
        let made = NewBranch {
            name: &branch,
            oid: base_oid,
        };
        if call.cancelled() {
            let ours = Leftovers {
                worktree: false,
                branch_lock: false,
            };
            let error = WorktreeError::Cancelled;
            return Err(self.undo_failed_add(&common, &path, made, ours, "was cancelled", error));
        }
        let args: [&OsStr; 8] = [
            "-c".as_ref(),
            NO_REF_LOCK_WAIT.as_ref(),
            "worktree".as_ref(),
            "add".as_ref(),
            "--quiet".as_ref(),
            "--no-guess-remote".as_ref(),
            path.as_os_str(),
            branch.as_str().as_ref(),
        ];
        // A git that exits on its own releases its locks, so a lock of the new branch left after that belongs to
        // another git process. One killed by a cancel or a signal may have died holding it: the add updates the
        // branch (to where it already points) as it checks it out. It never waits for another process's lock
        // (`NO_REF_LOCK_WAIT`), so it can't have been killed while waiting on one.
        let (failure, killed) = match self.run(dir, &args, None, Some(&call)) {
            // Cancelled just as it finished on its own: undone all the same, as a cancelled create makes nothing.
            Ok(_) if call.cancelled() => (Failure::Cancelled, false),
            Ok(_) => {
                return Ok(Created {
                    path: WorktreePath::new(path),
                    branch,
                    base,
                });
            }
            Err(failure) => {
                let killed = matches!(
                    failure,
                    Failure::Timeout | Failure::Cancelled | Failure::Exit { code: None, .. }
                );
                (failure, killed)
            }
        };
        let ours = Leftovers {
            worktree: !registered_before,
            branch_lock: killed,
        };
        let (what, error) = match failure {
            Failure::Cancelled => ("was cancelled".to_owned(), WorktreeError::Cancelled),
            other => {
                let failure = other.describe(self.git_timeout);
                let error = WorktreeError::Other(format!("git worktree add: {failure}"));
                (format!("failed ({failure})"), error)
            }
        };
        Err(self.undo_failed_add(&common, &path, made, ours, &what, error))
    }

    /// Kills the git call the create is waiting on, if any (its own thread then waits for the group to be gone and
    /// cleans up); a create between git calls stops before its next one.
    fn cancel_create(&self, repo: &RepoRef, name: &WorktreeName) {
        let key = (repo.as_path().to_owned(), name.as_str().to_owned());
        let mut creates = lock(&self.creates);
        let call = creates.entry(key).or_default();
        call.cancelled = true;
        if let Some(pgid) = call.pgid {
            kill_group(pgid);
        }
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
    /// Killed because its create was cancelled.
    Cancelled,
    Exit {
        code: Option<i32>,
        stderr: String,
    },
}

impl Failure {
    fn describe(&self, timeout: Duration) -> String {
        match self {
            Self::Io(e) => format!("can't run git: {e}"),
            Self::Timeout => format!("timed out after {timeout:?}"),
            Self::Cancelled => "cancelled".to_owned(),
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
            Self::Cancelled => WorktreeError::Cancelled,
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

/// What a failed `worktree add` may have left behind that is ours to remove, besides the new branch.
#[derive(Debug, Clone, Copy)]
struct Leftovers {
    /// The worktree git lists at the path, and its directory.
    worktree: bool,
    /// The new branch's ref lock.
    branch_lock: bool,
}

/// The branch `create` made, and the commit it made it at.
#[derive(Debug, Clone, Copy)]
struct NewBranch<'a> {
    name: &'a BranchName,
    oid: &'a str,
}

/// A checkout as [`GitWorktrees::checkout_of`] finds it.
#[derive(Debug)]
struct Placement {
    top: PathBuf,
    git_dir: PathBuf,
    common: PathBuf,
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
        // SAFETY: plain syscall. While the leader is unreaped or the group has members, the group id still names our
        // group; if the group has no members left it fails with ESRCH, which is fine.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

/// Blocks until child `pid` has exited, leaving it a zombie (`WNOWAIT`) so its pid stays taken, and returns how it
/// exited: the `si_code` of `waitid`, e.g. `CLD_EXITED` or `CLD_KILLED`.
fn wait_unreaped(pid: u32) -> io::Result<libc::c_int> {
    let pid = libc::id_t::from(pid);
    loop {
        // SAFETY: `info` is a valid out-pointer; `pid` is our own unreaped child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 {
            return Ok(info.si_code);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether a child that exited as `code` (from [`wait_unreaped`]) was killed by a signal.
fn killed_by_signal(code: libc::c_int) -> bool {
    code == libc::CLD_KILLED || code == libc::CLD_DUMPED
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
        BaseSpec, BaseUsed, BranchName, Created, RepoRef, WorktreeError, WorktreeName,
        WorktreePath, Worktrees,
    };
    use shepherd_testkit::contracts::worktrees::{
        DEFAULT, FsWorktreesFixture, HeldCreate, NESTED, REPO_NAME, SUBDIR, WorktreesFixture,
    };
    use tempfile::TempDir;

    use super::{Failure, GitWorktrees};

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
            std::fs::create_dir_all(seed.join(SUBDIR).join(NESTED)).unwrap();
            std::fs::write(seed.join(SUBDIR).join("lib.txt"), "lib\n").unwrap();
            std::fs::write(seed.join(SUBDIR).join(NESTED).join("deep.txt"), "deep\n").unwrap();
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

        fn resolved(&self, path: &Path) -> PathBuf {
            std::fs::canonicalize(path).unwrap()
        }

        fn hold_next_create(&self) -> Box<dyn HeldCreate + '_> {
            Box::new(HeldHook::install(self, ""))
        }
    }

    /// A `post-checkout` hook (run after git registered the worktree and made its branch) that runs a setup script,
    /// opens the gate FIFO, says it has started on another FIFO, then reads the gate until it ends. The fixture holds
    /// the gate's only write end until it is dropped, which ends the read of every hook that has said it started.
    /// Dropping it also uninstalls the hook.
    struct HeldHook<'a> {
        f: &'a GitFixture,
        entered: PathBuf,
        /// The gate's write end (opened read-write, so opening it doesn't wait for a reader).
        _gate: std::fs::File,
    }

    impl<'a> HeldHook<'a> {
        fn install(f: &'a GitFixture, setup: &str) -> Self {
            let n = f.counter.fetch_add(1, Ordering::Relaxed);
            let entered = f.tmp().join(format!("entered-{n}.fifo"));
            let gate = f.tmp().join(format!("gate-{n}.fifo"));
            for fifo in [&entered, &gate] {
                let status = Command::new("mkfifo").arg(fifo).status().unwrap();
                assert!(status.success());
            }
            let writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&gate)
                .unwrap();
            hook(
                f,
                "post-checkout",
                &format!(
                    "{setup}\nexec 3< '{}'\necho entered > '{}'\ncat <&3 > /dev/null",
                    gate.display(),
                    entered.display()
                ),
            );
            Self {
                f,
                entered,
                _gate: writer,
            }
        }
    }

    impl HeldCreate for HeldHook<'_> {
        /// Reads the `entered` FIFO on a helper thread, so a create that never reaches the hook fails the wait
        /// instead of hanging it. (That helper then stays blocked until the test process exits.)
        fn wait_entered(&self, limit: Duration) -> bool {
            let (tx, rx) = std::sync::mpsc::channel();
            let entered = self.entered.clone();
            std::thread::spawn(move || {
                let mut said = String::new();
                let ok = std::fs::File::open(&entered)
                    .and_then(|mut fifo| fifo.read_to_string(&mut said))
                    .is_ok();
                let _ = tx.send(ok && said.trim() == "entered");
            });
            rx.recv_timeout(limit).unwrap_or(false)
        }
    }

    impl Drop for HeldHook<'_> {
        /// Then the gate's write end closes (a field), ending the waiting hooks' reads.
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.f.app().join(".git/hooks/post-checkout"));
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

    /// A bare repo has no main checkout, so each of its worktrees names itself; the bare repo is not a checkout.
    #[test]
    fn repo_root_of_a_bare_repos_worktree_is_that_worktree() {
        let f = GitFixture::new();
        let origin = f.tmp().join("origin.git");
        let wt = f.tmp().join("bare-wt");
        f.git(
            &origin,
            &["worktree", "add", "--quiet", wt.to_str().unwrap(), DEFAULT],
        );
        let canonical = std::fs::canonicalize(&wt).unwrap();
        assert_eq!(f.port.repo_root(&wt.join(SUBDIR)), Ok(canonical));
        assert_eq!(
            f.port.repo_root(&origin),
            Err(WorktreeError::NotARepo(origin.clone()))
        );
    }

    /// The bare-repo-plus-worktrees layout: a bare clone in `proj/.git`. `proj` is not a checkout, so it can't name
    /// the repo, although its git dir is called `.git`.
    #[test]
    fn repo_root_of_a_worktree_of_a_bare_repo_named_dot_git_is_that_worktree() {
        let f = GitFixture::new();
        let proj = f.tmp().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let origin = f.tmp().join("origin.git");
        f.git(
            &proj,
            &[
                "clone",
                "--quiet",
                "--bare",
                origin.to_str().unwrap(),
                ".git",
            ],
        );
        f.git(
            &proj.join(".git"),
            &["worktree", "add", "--quiet", "../wt", DEFAULT],
        );
        let wt = proj.join("wt");
        let canonical = std::fs::canonicalize(&wt).unwrap();
        assert_eq!(f.port.repo_root(&wt.join(SUBDIR)), Ok(canonical.clone()));
        assert_eq!(f.port.repo_root(&canonical), Ok(canonical));
        assert_eq!(
            f.port.repo_root(&proj),
            Err(WorktreeError::NotARepo(proj.clone()))
        );
    }

    /// `git init --separate-git-dir <store>/.git <checkout>`: the checkout is the repo, not the git dir's parent.
    #[test]
    fn repo_root_of_a_checkout_with_a_separate_git_dir_is_the_checkout() {
        let f = GitFixture::new();
        let store = f.tmp().join("store");
        std::fs::create_dir(&store).unwrap();
        let git_dir = store.join(".git");
        f.git(
            &f.tmp(),
            &[
                "clone",
                "--quiet",
                "--separate-git-dir",
                git_dir.to_str().unwrap(),
                "origin.git",
                "sep",
            ],
        );
        let sep = f.tmp().join("sep");
        let canonical = std::fs::canonicalize(&sep).unwrap();
        assert_eq!(f.port.repo_root(&sep.join(SUBDIR)), Ok(canonical.clone()));
        assert_eq!(f.port.repo_root(&sep), Ok(canonical));
    }

    /// A main checkout whose git dir `<store>/.git` names it with `core.worktree`. git lists `<store>` as the main
    /// checkout of a linked worktree, but `<store>` is not a checkout's top level (its top level is the checkout), so
    /// the worktree names itself rather than trusting either.
    #[test]
    fn repo_root_of_a_worktree_of_a_repo_with_core_worktree_set_is_that_worktree() {
        let f = GitFixture::new();
        let store = f.tmp().join("store");
        let checkout = f.tmp().join("checkout");
        std::fs::create_dir(&store).unwrap();
        std::fs::create_dir(&checkout).unwrap();
        let git_dir = store.join(".git");
        f.git(
            &f.tmp(),
            &[
                "clone",
                "--quiet",
                "--bare",
                "origin.git",
                git_dir.to_str().unwrap(),
            ],
        );
        f.git(&git_dir, &["config", "core.bare", "false"]);
        f.git(
            &git_dir,
            &["config", "core.worktree", checkout.to_str().unwrap()],
        );
        let wt = f.tmp().join("wt");
        let wt_arg = wt.to_str().unwrap();
        f.git(
            &git_dir,
            &["worktree", "add", "--quiet", "-b", "wt", wt_arg],
        );
        let listed = f.git(&wt, &["worktree", "list", "--porcelain"]);
        let store_canonical = std::fs::canonicalize(&store).unwrap();
        assert!(
            listed.starts_with(&format!("worktree {}\n", store_canonical.display())),
            "{listed}"
        );
        let canonical = std::fs::canonicalize(&wt).unwrap();
        assert_eq!(f.port.repo_root(&wt.join(SUBDIR)), Ok(canonical));
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

    /// The bound on short git calls in the tests that need one to fire: comfortably more than a metadata call takes
    /// under load. Only that bound firing is timing-dependent.
    const SHORT_TIMEOUT: Duration = Duration::from_secs(30);

    /// A port whose short calls (everything but fetch and `worktree add`) time out after [`SHORT_TIMEOUT`]. Cleanup
    /// keeps the generous test timeout.
    fn slow_port(f: &GitFixture) -> GitWorktrees {
        port(f.dir.path(), TEST_TIMEOUT).with_git_timeout(SHORT_TIMEOUT)
    }

    /// A liveness bound for a held create to reach its hook, never a measurement.
    const HELD_LIMIT: Duration = Duration::from_secs(600);

    /// Creates `n` with a `post-checkout` hook that runs `setup` and then blocks; once the hook is running, cancels
    /// the create from another thread, and returns what the create returned.
    fn cancelled_create(f: &GitFixture, n: &str, setup: &str) -> Result<Created, WorktreeError> {
        let held = HeldHook::install(f, setup);
        let repo = f.repo();
        std::thread::scope(|s| {
            let create = s.spawn(|| f.port.create(&repo, &name(n), BaseSpec::OriginDefault));
            assert!(held.wait_entered(HELD_LIMIT), "the hook never ran");
            f.port.cancel_create(&repo, &name(n));
            create.join().unwrap()
        })
    }

    /// `worktree add` held in its hook outlives the bound on every other git call, and isn't killed. The witness: a
    /// short call made on the same port after the hook started runs into that bound first.
    #[test]
    fn worktree_add_has_no_time_cap() {
        let f = GitFixture::new();
        let held = HeldHook::install(&f, "");
        let port = slow_port(&f);
        let repo = f.repo();
        let created = std::thread::scope(|s| {
            let create = s.spawn(|| port.create(&repo, &name("long"), BaseSpec::OriginDefault));
            assert!(held.wait_entered(HELD_LIMIT), "the hook never ran");
            let witness = port.git(&f.app(), &["-c", "alias.nap=!sleep 900", "nap"]);
            assert!(matches!(witness, Err(Failure::Timeout)), "{witness:?}");
            assert!(!create.is_finished());
            drop(held);
            create.join().unwrap()
        });
        let created = created.unwrap();
        assert!(created.path.as_path().is_dir());
        assert!(branch_exists(&f, "long"));
    }

    /// The hook and its `sleep` are in git's process group: a cancel kills them all, and waits for them to be gone.
    #[test]
    fn a_cancelled_create_leaves_no_hook_running() {
        let f = GitFixture::new();
        let pid_file = f.tmp().join("hook.pids");
        let setup = format!("sleep 900 &\necho \"$$ $!\" > '{}'", pid_file.display());
        let result = cancelled_create(&f, "orphans", &setup);
        assert_eq!(result, Err(WorktreeError::Cancelled));
        let pids: Vec<libc::pid_t> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        let alive: Vec<_> = pids.iter().copied().filter(|&pid| running(pid)).collect();
        for &pid in &alive {
            // SAFETY: plain syscall on a pid the hook reported.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(alive.is_empty(), "still running: {alive:?}");
        assert_nothing_left(&f, "orphans");
    }

    /// Two creates in one repo check out at the same time: the second gets to its hook while the first is held in its
    /// own.
    #[test]
    fn creates_in_one_repo_check_out_at_the_same_time() {
        let f = GitFixture::new();
        let held = HeldHook::install(&f, "");
        let repo = f.repo();
        std::thread::scope(|s| {
            let first = s.spawn(|| {
                f.port
                    .create(&repo, &name("first"), BaseSpec::OriginDefault)
            });
            assert!(held.wait_entered(HELD_LIMIT), "the first hook never ran");
            // The same hook file holds the second create on the same FIFOs.
            let second = s.spawn(|| {
                f.port
                    .create(&repo, &name("second"), BaseSpec::OriginDefault)
            });
            assert!(
                held.wait_entered(HELD_LIMIT),
                "the second create waited for the first"
            );
            drop(held);
            assert!(first.join().unwrap().is_ok());
            assert!(second.join().unwrap().is_ok());
        });
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
    fn a_cancelled_create_leaves_nothing_behind_and_can_be_retried() {
        let f = GitFixture::new();
        let result = cancelled_create(&f, "slow", "");
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert_nothing_left(&f, "slow");
        retry_succeeds(&f, "slow");
    }

    /// What an add killed while it makes the branch leaves behind.
    #[test]
    fn a_cancelled_create_removes_the_new_branchs_stale_lock() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/refs/heads/locked.lock");
        let result = cancelled_create(&f, "locked", &format!("touch '{}'", lock.display()));
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert!(!lock.exists());
        assert_nothing_left(&f, "locked");
        retry_succeeds(&f, "locked");
    }

    /// Without its `.git` file (as when the add is killed before writing it) git refuses to remove the worktree.
    #[test]
    fn a_cancelled_create_deletes_a_worktree_git_refuses_to_remove() {
        let f = GitFixture::new();
        let keep = create(&f, "keep");
        let keep_head = f.head(keep.as_path());
        let path = worktree_path(&f, "broken");
        let setup = format!("rm '{}'", path.join(".git").display());
        let result = cancelled_create(&f, "broken", &setup);
        assert_eq!(result, Err(WorktreeError::Cancelled));
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
    fn a_cancelled_create_leaves_a_directory_git_does_not_list_alone() {
        let f = GitFixture::new();
        let path = worktree_path(&f, "taken");
        let admin = f.app().join(".git").join("worktrees").join("taken");
        let theirs = path.join("theirs.txt");
        let setup = format!(
            "rm -rf '{admin}' '{path}'\nmkdir '{path}'\ntouch '{theirs}'",
            admin = admin.display(),
            path = path.display(),
            theirs = theirs.display(),
        );
        let result = cancelled_create(&f, "taken", &setup);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("was cancelled"), "{message}");
        assert!(message.contains("wasn't created by shepherd"), "{message}");
        assert!(message.contains("left alone"), "{message}");
        assert!(!message.contains("by hand"), "{message}");
        assert!(theirs.exists());
        assert!(!branch_exists(&f, "taken"));
    }

    /// `packed-refs.lock` (not ours to remove) makes deleting the new branch fail.
    #[test]
    fn a_cancelled_create_whose_cleanup_fails_says_so() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/packed-refs.lock");
        let result = cancelled_create(&f, "stuck", &format!("touch '{}'", lock.display()));
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("was cancelled"), "{message}");
        assert!(message.contains("can't delete branch stuck"), "{message}");
    }

    /// `post-checkout` runs after git has registered the worktree and made its branch; its exit code is the add's.
    #[test]
    fn a_create_whose_hook_fails_leaves_nothing_behind_and_can_be_retried() {
        let f = GitFixture::new();
        hook(&f, "post-checkout", "exit 1");
        let result = f
            .port
            .create(&f.repo(), &name("failed"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("worktree add"), "{message}");
        assert_nothing_left(&f, "failed");
        retry_succeeds(&f, "failed");
    }

    /// The new branch's lock held by another git process makes making the branch fail before anything is created.
    /// Cleanup would delete that lock, which isn't ours.
    #[test]
    fn a_create_that_fails_before_creating_anything_cleans_up_nothing() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/refs/heads/busy.lock");
        std::fs::write(&lock, "").unwrap();
        let result = f
            .port
            .create(&f.repo(), &name("busy"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("can't create branch busy"), "{message}");
        assert!(!message.contains("clean"), "{message}");
        assert!(lock.exists());
        assert_nothing_left(&f, "busy");
    }

    /// With `core.filesRefLockTimeout=-1` git would wait forever on another process's lock of the new branch, be
    /// killed by the timeout, and that lock would look like one it died holding.
    #[test]
    fn a_create_blocked_by_a_foreign_branch_lock_leaves_it_alone_whatever_the_lock_timeout() {
        let f = GitFixture::new();
        f.git(&f.app(), &["config", "core.filesRefLockTimeout", "-1"]);
        let lock = f.app().join(".git/refs/heads/busy.lock");
        std::fs::write(&lock, "").unwrap();
        let result = slow_port(&f).create(&f.repo(), &name("busy"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("can't create branch busy"), "{message}");
        assert!(lock.exists());
        assert_nothing_left(&f, "busy");
    }

    /// The same for the add: another process locks the new branch right after `create` made it (here a
    /// `reference-transaction` hook run as the branch is committed). The add must not wait on that lock and then be
    /// killed, which would make the lock look like its own.
    #[test]
    fn an_add_blocked_by_a_foreign_branch_lock_leaves_it_alone_whatever_the_lock_timeout() {
        let f = GitFixture::new();
        f.git(&f.app(), &["config", "core.filesRefLockTimeout", "-1"]);
        let lock = f.app().join(".git/refs/heads/held.lock");
        let once = f.tmp().join("held.once");
        hook(
            &f,
            "reference-transaction",
            &format!(
                "[ \"$1\" = committed ] || exit 0\n\
                 grep -q ' refs/heads/held$' || exit 0\n\
                 [ -e '{once}' ] && exit 0\n\
                 touch '{once}' '{lock}'",
                once = once.display(),
                lock = lock.display()
            ),
        );
        let result = slow_port(&f).create(&f.repo(), &name("held"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("worktree add failed"), "{message}");
        assert!(message.contains("can't delete branch held"), "{message}");
        assert!(lock.exists(), "{message}");
        assert!(!worktree_path(&f, "held").exists(), "{message}");
    }

    /// Someone else's worktree used to be at the path and its directory is gone, but git still lists it, so the add
    /// fails. That record isn't ours to remove.
    #[test]
    fn a_create_refused_by_a_stale_worktree_record_leaves_the_record_alone() {
        let f = GitFixture::new();
        let path = worktree_path(&f, "stale");
        let path_arg = path.to_str().unwrap();
        f.git(&f.app(), &["worktree", "add", "-b", "theirs", path_arg]);
        std::fs::remove_dir_all(&path).unwrap();
        let result = f
            .port
            .create(&f.repo(), &name("stale"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(!message.contains("clean"), "{message}");
        let listed = f.git(&f.app(), &["worktree", "list", "--porcelain"]);
        assert!(listed.contains("refs/heads/theirs"), "{listed}");
        assert!(!branch_exists(&f, "stale"));
    }

    /// A git killed by a signal (here by its own hook) may have died holding the new branch's lock.
    #[test]
    fn a_create_whose_git_is_killed_removes_the_new_branchs_stale_lock() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/refs/heads/killed.lock");
        hook(
            &f,
            "post-checkout",
            &format!("touch '{}'\nkill -KILL $PPID", lock.display()),
        );
        let result = f
            .port
            .create(&f.repo(), &name("killed"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("killed by a signal"), "{message}");
        assert!(!lock.exists());
        assert_nothing_left(&f, "killed");
        retry_succeeds(&f, "killed");
    }

    /// Whether `pid` is a live process (a zombie counts as dead).
    fn running(pid: libc::pid_t) -> bool {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    /// The hook and its `sleep` are in git's process group; killing git leaves them running unless the group is
    /// killed too.
    #[test]
    fn a_create_whose_git_is_killed_leaves_no_hook_running() {
        let f = GitFixture::new();
        let pid_file = f.tmp().join("hook.pids");
        hook(
            &f,
            "post-checkout",
            &format!(
                "sleep 900 &\necho \"$$ $!\" > '{}'\nkill -KILL $PPID\nwait",
                pid_file.display()
            ),
        );
        let result = f
            .port
            .create(&f.repo(), &name("orphans"), BaseSpec::OriginDefault);
        assert!(result.is_err(), "{result:?}");
        let pids: Vec<libc::pid_t> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        let alive: Vec<_> = pids.iter().copied().filter(|&pid| running(pid)).collect();
        for &pid in &alive {
            // SAFETY: plain syscall on a pid the hook reported.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(alive.is_empty(), "still running: {alive:?}");
    }

    /// Another process makes the branch while the fetch runs (here a `reference-transaction` hook, run when the
    /// fetch updates the remote-tracking ref), after `create` checked for it and right before it makes the branch:
    /// nothing checks again in between, so making it is what finds it. That branch isn't ours to delete.
    #[test]
    fn a_create_whose_branch_appears_during_the_fetch_leaves_that_branch_alone() {
        let f = GitFixture::new();
        f.advance_origin();
        hook(
            &f,
            "reference-transaction",
            "[ \"$1\" = committed ] || exit 0\n\
             grep -q ' refs/remotes/origin/' || exit 0\n\
             git update-ref -m theirs refs/heads/raced HEAD",
        );
        let result = f
            .port
            .create(&f.repo(), &name("raced"), BaseSpec::OriginDefault);
        assert!(branch_exists(&f, "raced"), "{result:?}");
        let reflog = f.git(&f.app(), &["reflog", "show", "refs/heads/raced"]);
        assert!(reflog.contains("theirs"), "{reflog}");
        assert_eq!(
            result,
            Err(WorktreeError::BranchExists(BranchName::new("raced")))
        );
        assert!(!worktree_path(&f, "raced").exists());
    }

    /// Someone moves the new branch after `create` made it (here the hook, which then fails the add). It now holds
    /// their commit, so it isn't ours to delete.
    #[test]
    fn a_failed_create_leaves_a_branch_someone_moved_alone() {
        let f = GitFixture::new();
        f.advance_origin();
        hook(
            &f,
            "post-checkout",
            "git update-ref -m theirs refs/heads/moved HEAD~1\nexit 1",
        );
        let result = f
            .port
            .create(&f.repo(), &name("moved"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(branch_exists(&f, "moved"), "{message}");
        let theirs = f.git(&f.app(), &["rev-parse", &format!("origin/{DEFAULT}~1")]);
        assert_eq!(f.git(&f.app(), &["rev-parse", "refs/heads/moved"]), theirs);
        assert!(message.contains("worktree add failed"), "{message}");
        assert!(message.contains("branch moved"), "{message}");
        assert!(message.contains("left alone"), "{message}");
        assert!(!message.contains("by hand"), "{message}");
        assert!(!worktree_path(&f, "moved").exists());
    }

    /// The same after a cancel.
    #[test]
    fn a_cancelled_create_leaves_a_branch_someone_moved_alone() {
        let f = GitFixture::new();
        f.advance_origin();
        let setup = "git update-ref -m theirs refs/heads/moved HEAD~1";
        let result = cancelled_create(&f, "moved", setup);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("was cancelled"), "{message}");
        assert!(message.contains("left alone"), "{message}");
        let theirs = f.git(&f.app(), &["rev-parse", &format!("origin/{DEFAULT}~1")]);
        assert_eq!(f.git(&f.app(), &["rev-parse", "refs/heads/moved"]), theirs);
        assert!(!worktree_path(&f, "moved").exists());
    }

    /// Making the branch itself hangs (here in a `reference-transaction` hook, which runs while git holds the new
    /// branch's lock), so git is killed holding that lock.
    #[test]
    fn a_create_whose_branch_creation_times_out_leaves_nothing_behind_and_can_be_retried() {
        let f = GitFixture::new();
        let fifo = f.tmp().join("never.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        hook(
            &f,
            "reference-transaction",
            &format!(
                "[ \"$1\" = prepared ] || exit 0\n\
                 grep -q ' refs/heads/hung$' || exit 0\n\
                 cat '{}' > /dev/null",
                fifo.display()
            ),
        );
        let result = slow_port(&f).create(&f.repo(), &name("hung"), BaseSpec::OriginDefault);
        assert_eq!(result, Err(WorktreeError::Timeout("update-ref".into())));
        assert!(!f.app().join(".git/refs/heads/hung.lock").exists());
        assert_nothing_left(&f, "hung");
        std::fs::remove_file(f.app().join(".git/hooks/reference-transaction")).unwrap();
        retry_succeeds(&f, "hung");
    }

    /// `packed-refs.lock` (not ours to remove) makes deleting the new branch fail.
    #[test]
    fn a_failed_create_whose_cleanup_fails_says_so() {
        let f = GitFixture::new();
        let lock = f.app().join(".git/packed-refs.lock");
        hook(
            &f,
            "post-checkout",
            &format!("touch '{}'\nexit 1", lock.display()),
        );
        let result = f
            .port
            .create(&f.repo(), &name("stuck"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("worktree add failed"), "{message}");
        assert!(message.contains("can't delete branch stuck"), "{message}");
        assert!(lock.exists());
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
