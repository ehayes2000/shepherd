//! `Worktrees` over the `git` CLI. Every call runs `git -C <dir> ...` and parses only porcelain or plumbing output.
//!
//! # Never waiting on a person
//!
//! Each git process starts a new session (`setsid`), so it has no controlling terminal and neither do its children:
//! ssh can't open `/dev/tty` to ask for a passphrase or host key. Stdin is closed, and the environment turns off the
//! prompts that don't need a terminal: an empty `GIT_ASKPASS` (which also overrides `core.askPass` and `SSH_ASKPASS`,
//! so an editor's askpass helper never runs), `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` (Git Credential
//! Manager) and `SSH_ASKPASS_REQUIRE=never`. A process that runs past its timeout is killed with its whole process group, hooks and
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
//! - `fetch`: `--no-tags --no-prune --no-recurse-submodules --no-auto-gc` (beats `remote.origin.tagOpt`,
//!   `fetch.prune`, `fetch.pruneTags`, `fetch.recurseSubmodules`, `gc.auto`), and the refspec and locking below.
//! - `worktree add`: `--no-guess-remote` (beats `worktree.guessRemote`). The branch it checks out is made with
//!   `update-ref`, which never sets an upstream (so `branch.autoSetupMerge` doesn't apply).
//! - `update-ref` (making and deleting the new branch) and `worktree add`: `-c core.filesRefLockTimeout=0`, so they
//!   never wait on another process's lock of the new branch, and a lock left after one of them is killed is its own.
//! - `fetch`, continued: an explicit refspec `+refs/heads/<default>:refs/shepherd/fetch/<worktree name>` into a ref of
//!   the create's own, with `--refmap=` (beats a custom, single-branch or extra `remote.origin.fetch`, whose
//!   remote-tracking refs git would otherwise update, and lock, too; `remote.origin.followRemoteHEAD` only applies to the configured refspecs) and
//!   `-c core.filesRefLockTimeout=0`. So the only ref it locks is one no other process locks: a fetch killed by its
//!   timeout or a cancel may die holding that lock, which is then removed, unless it was there before the fetch
//!   started. The private ref is deleted once its commit has been read, whichever way the create goes. Then
//!   `refs/remotes/origin/<default>` is moved to the fetched commit with a short `update-ref` (pinned the same way,
//!   never cancelled), only from the value it had before the fetch; if another process holds its lock, or moved it
//!   meanwhile, that fails at once and the worktree starts from the fetched commit all the same. A lock of a
//!   `refs/remotes/*` ref is never removed.
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
use std::time::Duration;

use shepherd_core::ports::worktrees::{
    managed_parts, normalize_lexically, repo_name_from_common_dir,
};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, Created, RepoRef, Worktree, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
};

use crate::process_group::{kill_group, killed_by_signal, wait_group_gone, wait_unreaped};

/// For every git call except fetch and `worktree add`: short metadata calls (rev-parse, show-ref, update-ref, worktree
/// list, ...), so generous. `worktree add` has no time cap: it checks out files (a big repo's checkout with git-lfs
/// smudging can take many minutes) and runs the user's hooks; [`Worktrees::cancel_create`] stops it.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// For the calls that lock a ref (`update-ref` making the new branch or moving `origin/<default>`, `worktree add`,
/// deleting the new branch in cleanup, and the fetch into its private ref): fail at once on a lock that is already taken instead of waiting (`core.filesRefLockTimeout`),
/// so a lock left after such a call is killed is not one it was waiting on, and cleanup doesn't hang on someone else's.
const NO_REF_LOCK_WAIT: &str = "core.filesRefLockTimeout=0";

/// Where a create's fetch lands: `<namespace><worktree name>`, a ref of that create's own (one create of a name runs at
/// a time), deleted once its commit has been read.
const FETCH_NAMESPACE: &str = "refs/shepherd/fetch/";

/// The ref a create's fetch lands in. Deleted when this is dropped, whichever way the create goes.
struct FetchedRef<'a> {
    port: &'a GitWorktrees,
    dir: &'a Path,
    name: String,
}

impl Drop for FetchedRef<'_> {
    fn drop(&mut self) {
        // Best effort: a fetch that failed made nothing to delete, and a ref left behind is overwritten by the next
        // fetch of its name.
        let delete = ["-c", NO_REF_LOCK_WAIT, "update-ref", "-d", &self.name];
        let _ = self
            .port
            .git_within(self.dir, &delete, self.port.cleanup_timeout);
    }
}

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

    /// Runs a git call that finds `dir`'s repo, for [`Failure::or_not_a_repo`], which reads git's messages: so they
    /// are untranslated (`LC_ALL=C`). Only these calls get the C locale: the hooks other calls run keep the user's.
    fn probe<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<String, Failure> {
        self.run_with(dir, args, Some(self.git_timeout), None, &[("LC_ALL", "C")])
    }

    fn run<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Option<Duration>,
        call: Option<&CreateCall>,
    ) -> Result<String, Failure> {
        self.run_with(dir, args, timeout, call, &[])
    }

    /// Runs git in `dir`, with `vars` set last, killing it (its whole process group) once `timeout` passes, if there
    /// is one, or once `call` is cancelled, if it belongs to a create. Either way the group is gone when this returns.
    fn run_with<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Option<Duration>,
        call: Option<&CreateCall>,
        vars: &[(&str, &str)],
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
            // Empty, not unset: git then skips `core.askPass` and `SSH_ASKPASS` too, and with terminal prompts off
            // fails instead of asking.
            .env("GIT_ASKPASS", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .envs(vars.iter().copied());
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
            .probe(
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
            .probe(path, &args)
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

    /// The object id `full_ref` holds, or `None` if it doesn't exist.
    fn ref_value(&self, dir: &Path, full_ref: &str) -> Result<Option<String>, WorktreeError> {
        match self.git(dir, &["rev-parse", "--verify", "--quiet", full_ref]) {
            Ok(out) => Ok(Some(out.trim_end_matches('\n').to_owned())),
            Err(Failure::Exit { .. }) => Ok(None),
            Err(f) => Err(f.into_error("rev-parse")),
        }
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
        // The fetch lands in a ref of this create's own, the only ref it locks (`--refmap=`), which no other process
        // locks: a lock of it there before the fetch is a leftover it fails on at once (`NO_REF_LOCK_WAIT`), and one
        // there after a killed fetch that wasn't before is the fetch's own.
        let fetched = FetchedRef {
            port: self,
            dir,
            name: format!("{FETCH_NAMESPACE}{name}"),
        };
        let fetched_lock = common.join(format!("{}.lock", fetched.name));
        let lock_before = fetched_lock.symlink_metadata().is_ok();
        // What `origin/<default>` holds before the fetch (`None`: it doesn't exist), so moving it afterwards never
        // undoes someone else's move made meanwhile.
        let tracking = format!("refs/remotes/origin/{default}");
        let tracking_before = self.ref_value(dir, &tracking)?;
        let fetch = self.run(
            dir,
            &[
                "-c",
                NO_REF_LOCK_WAIT,
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-prune",
                "--no-recurse-submodules",
                "--no-auto-gc",
                "--refmap=",
                "origin",
                &format!("+refs/heads/{default}:{}", fetched.name),
            ],
            Some(self.fetch_timeout),
            Some(&call),
        );
        // Killed (timed out, cancelled, or by a signal), so it may have died holding that lock, which would keep its
        // ref from being deleted and fail the next fetch of this name.
        let left_lock = match &fetch {
            Err(failure) if failure.killed() && !lock_before => {
                remove_stale_lock(&fetched_lock).err()
            }
            _ => None,
        };
        let (start, base) = match fetch {
            Ok(_) => (
                fetched.name.clone(),
                BaseUsed::Origin {
                    branch: BranchName::new(&default),
                },
            ),
            Err(Failure::Cancelled) => {
                return Err(match left_lock {
                    None => WorktreeError::Cancelled,
                    Some(cant) => {
                        WorktreeError::Other(format!("git fetch was cancelled, and {cant}"))
                    }
                });
            }
            Err(failure) => {
                let mut fetch_error = failure.describe(self.fetch_timeout);
                if let Some(cant) = left_lock {
                    fetch_error = format!("{fetch_error}, and {cant}");
                }
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
        if matches!(base, BaseUsed::Origin { .. }) {
            // Moves `origin/<default>` to what was fetched, as a plain fetch would, unless it moved since it was read
            // (e.g. the user's own fetch advanced it: it isn't rewound) or another process holds its lock. Either way
            // this fails at once (the old value; `NO_REF_LOCK_WAIT`), and the worktree starts from the fetched commit
            // all the same. Never cancelled (it is short). Timing out means it waited, which it must never do: the
            // create fails, and a lock it may have left is left alone, as it can't be told from another process's.
            let old = tracking_before.as_deref().unwrap_or("");
            // `--no-deref`: should `origin/<default>` be a symbolic ref (say, to the user's own branch), this never
            // moves what it points at. (Finding the default branch refuses such a ref today, since git resolves
            // `origin/HEAD` through it; this doesn't rely on that.)
            let update = [
                "-c",
                NO_REF_LOCK_WAIT,
                "update-ref",
                "--no-deref",
                &tracking,
                base_oid,
                old,
            ];
            if let Err(Failure::Timeout) = self.git(dir, &update) {
                return Err(WorktreeError::Timeout("update-ref".into()));
            }
        }
        drop(fetched);
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
                let killed = failure.killed();
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
            .probe(&target, &["rev-parse", "--show-toplevel"])
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

/// Deletes the ref lock `lock` a killed git left behind, if there is one.
fn remove_stale_lock(lock: &Path) -> Result<(), String> {
    match std::fs::remove_file(lock) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(format!("can't delete {}: {e}", lock.display()))
        }
        _ => Ok(()),
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

    /// Whether git was killed rather than exiting on its own: it may have died holding a lock.
    fn killed(&self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Cancelled | Self::Exit { code: None, .. }
        )
    }

    fn into_error(self, op: &str) -> WorktreeError {
        match self {
            Self::Timeout => WorktreeError::Timeout(op.to_owned()),
            Self::Cancelled => WorktreeError::Cancelled,
            other => WorktreeError::Other(format!("git {op}: {}", other.describe(Duration::ZERO))),
        }
    }

    /// For the calls that find `dir`'s repo ([`GitWorktrees::probe`]): git's answer that there is none there is
    /// [`WorktreeError::NotARepo`]. That answer is git's fatal exit (128) because `dir` doesn't exist (git can't change
    /// to it), isn't in a repo, or is in one without a work tree (a bare repo is no checkout). Any other failure,
    /// such as a git killed by a signal or one that refuses a repo it doesn't trust, says nothing about `dir` (F48, F51).
    fn or_not_a_repo(self, dir: &Path, op: &str) -> WorktreeError {
        match self {
            Self::Exit {
                code: Some(FATAL_EXIT),
                stderr,
            } if !dir.is_dir() || says_there_is_no_repo(&stderr) => {
                WorktreeError::NotARepo(dir.to_owned())
            }
            other => other.into_error(op),
        }
    }
}

/// The exit code of git's `die`.
const FATAL_EXIT: i32 = 128;

/// Whether git's `stderr`, untranslated, says there is no repo, or no work tree, where it ran.
fn says_there_is_no_repo(stderr: &str) -> bool {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("fatal: "))
        .any(|why| {
            why.starts_with("not a git repository")
                || why == "this operation must be run in a work tree"
        })
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Read;
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
        CREATE_LIMIT, DEFAULT, FsWorktreesFixture, HeldCreate, NESTED, REPO_NAME, SUBDIR,
        WorktreesFixture, wait_held,
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
            self.git(
                &app,
                &["config", "core.sshCommand", "sh -c 'sleep 3600' --"],
            );
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

    fn mkfifo(path: &Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
    }

    /// Whether a hook said `entered` on a FIFO, which a helper thread reads to its end from the start. A writer that
    /// never comes leaves the helper blocked until the test process exits, not the test. One reader for the FIFO's
    /// whole life, so it can be asked again (see [`wait_held`]) without a second reader racing the first for the line.
    struct Entered {
        said: std::sync::mpsc::Receiver<bool>,
        seen: std::cell::Cell<bool>,
    }

    impl Entered {
        fn watch(fifo: &Path) -> Self {
            let (tx, said) = std::sync::mpsc::channel();
            let fifo = fifo.to_owned();
            std::thread::spawn(move || {
                let mut said = String::new();
                let ok = std::fs::File::open(&fifo)
                    .and_then(|mut fifo| fifo.read_to_string(&mut said))
                    .is_ok();
                let _ = tx.send(ok && said.trim() == "entered");
            });
            Self {
                said,
                seen: std::cell::Cell::new(false),
            }
        }
    }

    impl HeldCreate for Entered {
        fn wait_entered(&self, limit: Duration) -> bool {
            if !self.seen.get() {
                self.seen
                    .set(self.said.recv_timeout(limit).unwrap_or(false));
            }
            self.seen.get()
        }
    }

    /// A FIFO hooks wait at. A hook opens it (`exec 3< <path>`, which returns at once because this holds a write end)
    /// before it says it has started, then reads one byte from it ([`Gate::WAIT`]).
    ///
    /// The release is a byte per waiting hook, which stays in the FIFO for every hook that has it open, not the end of
    /// the FIFO: on macOS, a read that starts after the last writer closed may block forever, and a hook can say it
    /// started just before its read starts.
    struct Gate {
        path: PathBuf,
        writer: std::fs::File,
    }

    impl Gate {
        /// The shell line that makes a hook wait at the gate, on the fd 3 its `exec` opened.
        const WAIT: &str = "dd bs=1 count=1 <&3 > /dev/null 2>&1";

        fn new(path: PathBuf) -> Self {
            mkfifo(&path);
            // Read-write, so opening it doesn't wait for a reader.
            let writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            Self { path, writer }
        }

        /// Lets every hook waiting at the gate (up to 64) go on.
        fn release(mut self) {
            use std::io::Write;
            let _ = self.writer.write_all(&[b'x'; 64]);
        }
    }

    /// A `post-checkout` hook (run after git registered the worktree and made its branch) that runs a setup script,
    /// takes its place at a [`Gate`], says it has started on another FIFO, then waits at the gate. Every create that
    /// runs the hook while it is installed is held. Dropping it releases the gate and uninstalls the hook.
    struct HeldHook<'a> {
        f: &'a GitFixture,
        name: &'static str,
        entered: Entered,
        gate: Option<Gate>,
    }

    impl<'a> HeldHook<'a> {
        fn install(f: &'a GitFixture, setup: &str) -> Self {
            Self::install_as(f, "post-checkout", setup)
        }

        /// The same as hook `name`; `setup` may `exit 0` to let a run of it through unheld.
        fn install_as(f: &'a GitFixture, name: &'static str, setup: &str) -> Self {
            let n = f.counter.fetch_add(1, Ordering::Relaxed);
            let entered = f.tmp().join(format!("entered-{n}.fifo"));
            mkfifo(&entered);
            let gate = Gate::new(f.tmp().join(format!("gate-{n}.fifo")));
            hook(
                f,
                name,
                &format!(
                    "{setup}\nexec 3< '{}'\necho entered > '{}'\n{}",
                    gate.path.display(),
                    entered.display(),
                    Gate::WAIT
                ),
            );
            Self {
                f,
                name,
                entered: Entered::watch(&entered),
                gate: Some(gate),
            }
        }
    }

    impl HeldCreate for HeldHook<'_> {
        fn wait_entered(&self, limit: Duration) -> bool {
            self.entered.wait_entered(limit)
        }
    }

    impl Drop for HeldHook<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.f.app().join(".git/hooks").join(self.name));
            if let Some(gate) = self.gate.take() {
                gate.release();
            }
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

    /// Installs a hook in the clone (hooks are shared by its worktrees), in place of any it had: a stand-in, so macOS
    /// doesn't assess a new executable on its first run (F17, F45).
    fn hook(f: &GitFixture, hook: &str, script: &str) {
        let path = f.app().join(".git").join("hooks").join(hook);
        let _ = std::fs::remove_file(&path);
        crate::stand_in::install(&path, &format!("{script}\n"));
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
    /// under load, which on a heavily loaded machine can be most of a minute (F17). Only that bound firing is
    /// timing-dependent.
    const SHORT_TIMEOUT: Duration = Duration::from_secs(120);

    /// A port whose short calls (everything but fetch and `worktree add`) time out after [`SHORT_TIMEOUT`]. Cleanup
    /// keeps the generous test timeout.
    fn slow_port(f: &GitFixture) -> GitWorktrees {
        port(f.dir.path(), TEST_TIMEOUT).with_git_timeout(SHORT_TIMEOUT)
    }

    type Answer = std::sync::mpsc::Receiver<Result<Created, WorktreeError>>;

    /// The answer of a create started with its own channel, within [`CREATE_LIMIT`] (a liveness bound). If it doesn't
    /// come, `release` runs first (so the create's thread can finish and the scope end), then this panics.
    fn answer(answer: &Answer, release: impl FnOnce()) -> Result<Created, WorktreeError> {
        answer.recv_timeout(CREATE_LIMIT).unwrap_or_else(|_| {
            release();
            panic!("the create never answered");
        })
    }

    /// Waits for `held` to say the create answering on `answer` is held, as [`wait_held`] does within
    /// [`CREATE_LIMIT`]. If it isn't, `cancel` cancels the create and `held` is dropped (so the create's thread ends
    /// and the scope with it: a hook that reached the gate only after its release would wait there for good), then
    /// this panics with why.
    fn assert_held<H: HeldCreate>(held: &mut Option<H>, answer: &Answer, cancel: impl FnOnce()) {
        if let Err(why) = wait_held(held.as_ref().unwrap(), answer, CREATE_LIMIT) {
            cancel();
            drop(held.take());
            panic!("the hook never ran: {why}");
        }
    }

    /// Creates `n` with a `post-checkout` hook that runs `setup` and then blocks; once the hook is running, cancels
    /// the create from another thread, and returns what the create returned. A cancel that doesn't stop the create
    /// fails the test (after releasing the hook), never hangs it.
    fn cancelled_create(f: &GitFixture, n: &str, setup: &str) -> Result<Created, WorktreeError> {
        cancelled_while_held(f, n, HeldHook::install(f, setup))
    }

    /// Creates `n`, and once `held` says it has started, cancels the create from another thread; returns what the
    /// create returned, as for [`cancelled_create`].
    fn cancelled_while_held(
        f: &GitFixture,
        n: &str,
        held: impl HeldCreate,
    ) -> Result<Created, WorktreeError> {
        let repo = &f.repo();
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            s.spawn(move || tx.send(f.port.create(repo, &name(n), BaseSpec::OriginDefault)));
            let mut held = Some(held);
            assert_held(&mut held, &rx, || f.port.cancel_create(repo, &name(n)));
            f.port.cancel_create(repo, &name(n));
            answer(&rx, || drop(held.take()))
        })
    }

    /// `worktree add` held in its hook outlives the bound on every other git call, and isn't killed. The witness: a
    /// short call made on the same port after the hook started runs into that bound first.
    #[test]
    fn worktree_add_has_no_time_cap() {
        let f = GitFixture::new();
        let held = HeldHook::install(&f, "");
        let port = &slow_port(&f);
        let repo = &f.repo();
        let created = std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            s.spawn(move || tx.send(port.create(repo, &name("long"), BaseSpec::OriginDefault)));
            let mut held = Some(held);
            assert_held(&mut held, &rx, || port.cancel_create(repo, &name("long")));
            let witness = port.git(&f.app(), &["-c", "alias.nap=!sleep 900", "nap"]);
            let still_held = rx.try_recv().is_err();
            drop(held.take());
            assert!(matches!(witness, Err(Failure::Timeout)), "{witness:?}");
            assert!(still_held, "the add finished while its hook was held");
            answer(&rx, || {})
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
        let setup = format!(
            "{} &\necho \"$$ $!\" > '{}'",
            marked_sleep(900, &pid_file),
            pid_file.display()
        );
        let result = cancelled_create(&f, "orphans", &setup);
        assert_eq!(result, Err(WorktreeError::Cancelled));
        let pids: Vec<libc::pid_t> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        let alive: Vec<_> = pids.iter().copied().filter(|&pid| running(pid)).collect();
        // Only the sleep is killed, while it is ours. The hook's shell needs no kill: its gate is released by now, so
        // it ends by itself.
        for &pid in &alive {
            kill_if_ours(pid, &pid_file);
        }
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(alive.is_empty(), "still running: {alive:?}");
        assert_nothing_left(&f, "orphans");
    }

    /// Run from a hook (in git's process group and session) with a file to write to: leaves a `sleep` in git's group
    /// whose parent is outside it, in a group of its own. Once that `sleep` is killed, the parent starts another one in
    /// git's group (a member the kills missed, like one forked while they were delivered), then two seconds later reaps
    /// both and exits. Writes "<git's group> <parent>" once the first `sleep` is in the group. Each `sleep` is a perl
    /// with the file in its arguments, as the parent has, so [`kill_if_ours`] can tell them ours.
    const LINGERING_ZOMBIE: &str = r#"
use strict;
use POSIX ();
my $pids = shift;
my $group = getpgrp();
pipe(my $r, my $w) or die "pipe: $!";
my $parent = fork() // die "fork: $!";
if ($parent == 0) {
    POSIX::setpgid(0, 0) or die "setpgid: $!";
    my $dead = 0;
    $SIG{CHLD} = sub { $dead = 1 };
    my $child = fork() // die "fork: $!";
    if ($child == 0) {
        POSIX::setpgid(0, $group) or die "setpgid: $!";
        close $r;
        print $w "ready\n";
        close $w;
        exec "perl", "-e", "sleep 900", $pids;
        die "exec: $!";
    }
    close $r;
    close $w;
    select(undef, undef, undef, 0.05) until $dead;
    my $late = fork() // die "fork: $!";
    if ($late == 0) {
        POSIX::setpgid(0, $group) or exit 1;
        exec "perl", "-e", "sleep 900", $pids;
        exit 1;
    }
    select(undef, undef, undef, 2);
    waitpid($child, 0);
    waitpid($late, POSIX::WNOHANG());
    exit 0;
}
close $w;
my $ready = <$r>;
exit 1 unless defined $ready && $ready eq "ready\n";
open(my $out, '>', $pids) or die "open: $!";
print $out "$group $parent\n";
close $out;
"#;

    /// A cancel returns only once git's whole process group is gone, checked the moment it returns: on macOS a group
    /// whose last members are dying, or are zombies not yet reaped, answers EPERM rather than ESRCH and still holds its
    /// id.
    ///
    /// The witness is a member of git's group whose parent is outside it (in a group of its own, in git's session), so
    /// the kill leaves it a zombie its parent reaps only a while later; until then the group is there (EPERM on macOS).
    /// Meanwhile that parent puts a live member in the group which no earlier kill reached: the wait kills it too
    /// rather than running out its bound with it alive.
    #[test]
    fn a_cancelled_create_returns_once_its_process_group_is_gone() {
        let f = GitFixture::new();
        let pids = f.tmp().join("lingering.pids");
        let script = f.tmp().join("lingering.pl");
        std::fs::write(&script, LINGERING_ZOMBIE).unwrap();
        let setup = format!("perl '{}' '{}' || exit 1", script.display(), pids.display());
        // Reported, never asserted on: a create that took the group wait's whole bound returned through it.
        let started = Instant::now();
        let result = cancelled_create(&f, "grouped", &setup);
        let took = started.elapsed();
        let recorded = std::fs::read_to_string(&pids).unwrap();
        let mut recorded = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<libc::pid_t>().unwrap());
        let (pgid, parent) = (recorded.next().unwrap(), recorded.next().unwrap());
        // SAFETY: plain syscall; signal 0 only checks whether the group exists.
        let rc = unsafe { libc::killpg(pgid, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        let members = if rc == -1 && errno == Some(libc::ESRCH) {
            String::new()
        } else {
            group_members(pgid)
        };
        // Every process the script starts names its pid file in its arguments: the parent, and the sleeps it leaves
        // in git's group. Each is killed only while it is ours.
        kill_if_ours(parent, &pids);
        if rc == 0 {
            for member in group_members(pgid).lines() {
                if let Some(Ok(pid)) = member.split_whitespace().next().map(str::parse) {
                    kill_if_ours(pid, &pids);
                }
            }
        }
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert!(
            rc == -1 && errno == Some(libc::ESRCH),
            "group {pgid} still there: rc {rc}, errno {errno:?}; the create took {took:?} (the group wait gives up \
             after 10 s); members (pid pgid ppid stat command):\n{members}"
        );
    }

    /// The processes in group `pgid`, one `ps` line each.
    fn group_members(pgid: libc::pid_t) -> String {
        let out = Command::new("ps")
            .args(["-ax", "-o", "pid=,pgid=,ppid=,stat=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.split_whitespace().nth(1) == Some(pgid.to_string().as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Two creates in one repo check out at the same time: the second gets to its hook while the first is held in its
    /// own. Each hook says it started on a FIFO named after its worktree, so neither can answer for the other.
    #[test]
    fn creates_in_one_repo_check_out_at_the_same_time() {
        let f = GitFixture::new();
        let tmp = f.tmp();
        let fifo = |n: &str| tmp.join(format!("{n}.fifo"));
        mkfifo(&fifo("entered-first"));
        mkfifo(&fifo("entered-second"));
        let gate = Gate::new(fifo("gate"));
        hook(
            &f,
            "post-checkout",
            &format!(
                "exec 3< '{gate}'\n\
                 echo entered > '{tmp}/entered-'\"$(basename \"$PWD\")\"'.fifo'\n\
                 {wait}",
                gate = gate.path.display(),
                tmp = tmp.display(),
                wait = Gate::WAIT
            ),
        );
        let repo = &f.repo();
        let port = &f.port;
        let (first, second) = std::thread::scope(|s| {
            let start = |n: &'static str| {
                let (tx, rx) = std::sync::mpsc::channel();
                s.spawn(move || tx.send(port.create(repo, &name(n), BaseSpec::OriginDefault)));
                rx
            };
            let (first_entered, second_entered) = (
                Entered::watch(&fifo("entered-first")),
                Entered::watch(&fifo("entered-second")),
            );
            // A create not held in time is cancelled before the gate is released: its hook, reaching the gate only
            // after that, would wait there for good, and the scope with it.
            let first = start("first");
            if let Err(why) = wait_held(&first_entered, &first, CREATE_LIMIT) {
                port.cancel_create(repo, &name("first"));
                gate.release();
                panic!("the first hook never ran: {why}");
            }
            let second = start("second");
            let entered = wait_held(&second_entered, &second, CREATE_LIMIT);
            let first_held = first.try_recv().is_err();
            if entered.is_err() {
                port.cancel_create(repo, &name("second"));
            }
            gate.release();
            assert!(
                entered.is_ok(),
                "the second create waited for the first: {entered:?}"
            );
            assert!(
                first_held,
                "the first create finished while its hook was held"
            );
            (answer(&first, || {}), answer(&second, || {}))
        });
        assert!(first.is_ok(), "{first:?}");
        assert!(second.is_ok(), "{second:?}");
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
        // A short cleanup bound: cleanup's delete waiting on the foreign lock would time out, not hang.
        let port = slow_port(&f).with_cleanup_timeout(SHORT_TIMEOUT);
        let result = port.create(&f.repo(), &name("held"), BaseSpec::OriginDefault);
        let Err(WorktreeError::Other(message)) = &result else {
            panic!("{result:?}");
        };
        assert!(message.contains("worktree add failed"), "{message}");
        assert!(message.contains("can't delete branch held"), "{message}");
        assert!(!message.contains("timed out"), "{message}");
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

    /// A port whose `git` is a script that kills itself with SIGKILL, as a stray signal (say, one sent to a reused
    /// pid) would.
    fn port_whose_git_is_killed(f: &GitFixture) -> GitWorktrees {
        // The body is sourced by the stand-in's own `sh`, so `$$` is the process the port spawned.
        port_whose_git_runs(f, "kill -KILL $$\n", &[])
    }

    /// A port whose `git` is a stand-in running `body`, with `vars` added to its environment (before the port's own).
    fn port_whose_git_runs(f: &GitFixture, body: &str, vars: &[(&str, &str)]) -> GitWorktrees {
        let bin = f.tmp().join("stand-in-bin");
        std::fs::create_dir(&bin).unwrap();
        crate::stand_in::install(&bin.join("git"), body);
        let mut env = env(&f.tmp());
        env.push(("PATH".into(), bin.into()));
        env.extend(vars.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        GitWorktrees::new(f.root())
            .with_isolated_env(env)
            .with_git_timeout(TEST_TIMEOUT)
    }

    /// A `git` that exits with `code` after printing `message` to stderr.
    fn git_failing(code: u8, message: &str) -> String {
        let quoted = message.replace('\'', r"'\''");
        format!("printf '%s\\n' '{quoted}' >&2\nexit {code}\n")
    }

    /// git's answer when there is no repo, in the C locale.
    const NOT_A_REPO: &str = "fatal: not a git repository (or any of the parent directories): .git";

    /// Any other fatal error says nothing about whether `dir` is a repo: it is a failure (F51).
    #[test]
    fn repo_root_reports_another_fatal_git_error_as_a_failure_not_as_not_a_repo() {
        let f = GitFixture::new();
        let git = git_failing(128, "fatal: detected dubious ownership in repository");
        let result = port_whose_git_runs(&f, &git, &[]).repo_root(&f.app());
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("dubious ownership")),
            "{result:?}"
        );
    }

    /// Likewise for the calls that only ask for the repo's git dir.
    #[test]
    fn default_branch_reports_another_fatal_git_error_as_a_failure_not_as_not_a_repo() {
        let f = GitFixture::new();
        let git = git_failing(128, "fatal: detected dubious ownership in repository");
        let result = port_whose_git_runs(&f, &git, &[]).default_branch(&f.repo());
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("dubious ownership")),
            "{result:?}"
        );
    }

    /// Only git's own exit for it (128) carries that answer.
    #[test]
    fn a_not_a_repo_message_with_another_exit_code_is_a_failure() {
        let f = GitFixture::new();
        let result = port_whose_git_runs(&f, &git_failing(1, NOT_A_REPO), &[]).repo_root(&f.app());
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("not a git repository")),
            "{result:?}"
        );
    }

    /// git translates its messages: the port asks for them untranslated, whatever the user's locale.
    #[test]
    fn repo_root_is_not_a_repo_whatever_the_users_locale() {
        let f = GitFixture::new();
        let port = port_whose_git_runs(&f, &git_with_no_repo_translated(""), &GERMAN);
        assert_eq!(
            port.repo_root(&f.app()),
            Err(WorktreeError::NotARepo(f.app()))
        );
    }

    /// Likewise for the calls that only ask for the repo's git dir.
    #[test]
    fn default_branch_is_not_a_repo_whatever_the_users_locale() {
        let f = GitFixture::new();
        let port = port_whose_git_runs(&f, &git_with_no_repo_translated(""), &GERMAN);
        assert_eq!(
            port.default_branch(&f.repo()),
            Err(WorktreeError::NotARepo(f.repo().as_path().to_owned()))
        );
    }

    /// And for `remove`'s check that the path is a checkout's top level, made once the path is known to be in a repo.
    #[test]
    fn remove_is_not_a_repo_whatever_the_users_locale() {
        let f = GitFixture::new();
        let dir = f.root().join(REPO_NAME).join("not-a-checkout");
        std::fs::create_dir_all(&dir).unwrap();
        let common = f.app().join(".git");
        let answers = format!(
            "case \"$*\" in *--git-common-dir*) printf '%s\\n' '{}'; exit 0 ;; esac\n",
            common.display()
        );
        let port = port_whose_git_runs(&f, &git_with_no_repo_translated(&answers), &GERMAN);
        assert_eq!(
            port.remove(&WorktreePath::new(&dir), true),
            Err(WorktreeError::NotARepo(
                std::fs::canonicalize(&dir).unwrap()
            ))
        );
    }

    /// A user's locale in which git translates its messages.
    const GERMAN: [(&str, &str); 2] = [("LANG", "de_DE.UTF-8"), ("LC_ALL", "de_DE.UTF-8")];

    /// A `git` that runs `answers` (which may answer a call and exit), then says there is no repo: untranslated only if
    /// asked for the C locale.
    fn git_with_no_repo_translated(answers: &str) -> String {
        format!(
            "{answers}if [ \"$LC_ALL\" = C ]; then m='{NOT_A_REPO}'; else m='fatal: Kein Git-Repository'; fi\n\
             printf '%s\\n' \"$m\" >&2\nexit 128\n"
        )
    }

    /// Only git's own words carry that answer, not a path in another error that happens to contain them.
    #[test]
    fn a_fatal_error_naming_a_path_with_not_a_repo_in_it_is_a_failure() {
        let f = GitFixture::new();
        let git = git_failing(
            128,
            "fatal: detected dubious ownership in repository at '/x/not a git repository'",
        );
        let result = port_whose_git_runs(&f, &git, &[]).repo_root(&f.app());
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("dubious ownership")),
            "{result:?}"
        );
    }

    /// A git killed by a signal says nothing about the directory it ran in: it is no answer that it isn't a repo (F48).
    #[test]
    fn repo_root_reports_a_git_killed_by_a_signal_as_a_failure_not_as_not_a_repo() {
        let f = GitFixture::new();
        let result = port_whose_git_is_killed(&f).repo_root(&f.app().join(SUBDIR));
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("killed by a signal")),
            "{result:?}"
        );
    }

    /// Likewise for the calls that only ask for the repo's git dir.
    #[test]
    fn default_branch_reports_a_git_killed_by_a_signal_as_a_failure_not_as_not_a_repo() {
        let f = GitFixture::new();
        let result = port_whose_git_is_killed(&f).default_branch(&f.repo());
        assert!(
            matches!(&result, Err(WorktreeError::Other(m)) if m.contains("killed by a signal")),
            "{result:?}"
        );
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

    /// A shell command that sleeps for `secs` with `marker` (a path in the test's tempdir) in its arguments, so
    /// [`kill_if_ours`] can tell it is ours.
    fn marked_sleep(secs: u32, marker: &Path) -> String {
        format!("perl -e 'sleep {secs}' '{}'", marker.display())
    }

    /// Kills `pid` if it is a live process whose arguments name `marker`, a path in the test's tempdir (as a
    /// [`marked_sleep`]'s do). Once our process is gone its pid may name someone else's, such as a git another test
    /// started, which is left alone (F48, F51).
    fn kill_if_ours(pid: libc::pid_t, marker: &Path) {
        crate::test_pids::signal_if_ours(pid, marker, libc::SIGKILL);
    }

    /// A process that took the pid of one of ours is spared.
    #[test]
    fn kill_if_ours_spares_a_process_that_does_not_name_the_marker() {
        use std::os::unix::process::ExitStatusExt;
        let f = GitFixture::new();
        let mut stranger = Command::new("sleep").arg("300").spawn().unwrap();
        let id = libc::pid_t::try_from(stranger.id()).unwrap();
        kill_if_ours(id, &f.tmp());
        // A SIGKILL sent above would be acted on before this SIGTERM.
        // SAFETY: plain syscall on our own unreaped child.
        unsafe { libc::kill(id, libc::SIGTERM) };
        let status = stranger.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    }

    /// One that names the marker is killed.
    #[test]
    fn kill_if_ours_kills_a_marked_sleep() {
        use std::os::unix::process::ExitStatusExt;
        let f = GitFixture::new();
        let mut ours = Command::new("/bin/sh")
            .args(["-c", &format!("exec {}", marked_sleep(300, &f.tmp()))])
            .spawn()
            .unwrap();
        let id = libc::pid_t::try_from(ours.id()).unwrap();
        // `ps` shows the new arguments once `sh` has exec'd perl.
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !ps_args(id).contains("perl") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        kill_if_ours(id, &f.tmp());
        // So a missed kill fails the test rather than holding it: the SIGKILL, if sent, is acted on first.
        // SAFETY: plain syscall on our own unreaped child.
        unsafe { libc::kill(id, libc::SIGTERM) };
        let status = ours.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }

    /// The arguments `ps` shows for `pid`.
    fn ps_args(pid: libc::pid_t) -> String {
        let out = Command::new("ps")
            .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
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
                "{} &\necho \"$$ $!\" > '{}'\nkill -KILL $PPID\nwait",
                marked_sleep(900, &pid_file),
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
        // Only the sleep is killed, while it is ours. The hook's shell needs no kill: it waits for the sleep.
        for &pid in &alive {
            kill_if_ours(pid, &pid_file);
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

    /// The lock of `origin/<default>`.
    fn tracking_lock(f: &GitFixture) -> PathBuf {
        f.app()
            .join(format!(".git/refs/remotes/origin/{DEFAULT}.lock"))
    }

    /// Every ref and ref lock under `refs/shepherd/` (the fetch's own refs), as `for-each-ref` and the files show them.
    fn fetch_leftovers(f: &GitFixture) -> Vec<String> {
        let mut found: Vec<String> = f
            .git(
                &f.app(),
                &["for-each-ref", "--format=%(refname)", "refs/shepherd/"],
            )
            .lines()
            .map(str::to_owned)
            .collect();
        let dir = f.app().join(".git/refs/shepherd/fetch");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            found.extend(
                entries
                    .flatten()
                    .map(|e| e.path().display().to_string())
                    .filter(|p| p.ends_with(".lock")),
            );
        }
        found
    }

    /// The `reference-transaction` lines that pick the fetch's transaction once it has locked its refs (`prepared`): the
    /// one creating its ref (old value all zeros, new value not), not the one deleting it afterwards (new value all
    /// zeros). Origin moves first, so the fetch has an update to make.
    fn when_the_fetch_holds_its_locks(f: &GitFixture) -> String {
        f.advance_origin();
        "[ \"$1\" = prepared ] || exit 0\n\
         grep -qE '^0{40} 0*[1-9a-f][0-9a-f]* refs/shepherd/fetch/' || exit 0"
            .to_owned()
    }

    /// A create after a killed fetch fetches again: nothing it left behind fails that fetch, and its fetch leaves
    /// nothing behind either.
    fn next_create_fetches(f: &GitFixture, n: &str) {
        let created = f
            .port
            .create(&f.repo(), &name(n), BaseSpec::OriginDefault)
            .unwrap();
        let fetched = BaseUsed::Origin {
            branch: BranchName::new(DEFAULT),
        };
        assert_eq!(created.base, fetched);
        assert_eq!(f.head(created.path.as_path()), f.origin_tip());
        let tracking = format!("refs/remotes/origin/{DEFAULT}");
        assert_eq!(f.git(&f.app(), &["rev-parse", &tracking]), f.origin_tip());
        assert_eq!(fetch_leftovers(f), Vec::<String>::new());
    }

    #[test]
    fn a_cancelled_fetch_leaves_no_lock_or_ref_of_its_own_behind() {
        let f = GitFixture::new();
        let setup = when_the_fetch_holds_its_locks(&f);
        let held = HeldHook::install_as(&f, "reference-transaction", &setup);
        let result = cancelled_while_held(&f, "fetching", held);
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert_eq!(fetch_leftovers(&f), Vec::<String>::new());
        next_create_fetches(&f, "after");
    }

    /// Kills git while it holds its locks (from its own `reference-transaction` hook), and returns what the create
    /// returned. A timeout kills git the same way; it isn't the witness, since under load the fetch may not reach its
    /// locks before any timeout short enough for a test fires.
    fn create_with_fetch_killed(f: &GitFixture, n: &str) -> Created {
        let setup = when_the_fetch_holds_its_locks(f);
        hook(
            f,
            "reference-transaction",
            &format!("{setup}\nkill -KILL $PPID"),
        );
        let created = f.port.create(&f.repo(), &name(n), BaseSpec::OriginDefault);
        std::fs::remove_file(f.app().join(".git/hooks/reference-transaction")).unwrap();
        let created = created.unwrap();
        let BaseUsed::Stale { fetch_error, .. } = &created.base else {
            panic!("{:?}", created.base);
        };
        assert!(fetch_error.contains("killed by a signal"), "{fetch_error}");
        created
    }

    /// Killed rather than cancelled: the create goes on from the last fetched origin default, and nothing is left to
    /// fail the next fetch.
    #[test]
    fn a_killed_fetch_leaves_no_lock_or_ref_of_its_own_behind() {
        let f = GitFixture::new();
        create_with_fetch_killed(&f, "killed");
        assert_eq!(fetch_leftovers(&f), Vec::<String>::new());
        next_create_fetches(&f, "after");
    }

    /// git would also update (and lock) the remote-tracking refs an extra configured refspec maps the default branch
    /// to, in the same transaction, and a fetch killed holding that lock would fail every later one.
    #[test]
    fn a_killed_fetch_with_an_extra_configured_refspec_leaves_no_lock_behind() {
        let f = GitFixture::new();
        let mirror = "+refs/heads/*:refs/remotes/mirror/*";
        f.git(
            &f.app(),
            &["config", "--add", "remote.origin.fetch", mirror],
        );
        create_with_fetch_killed(&f, "killed");
        let mirror_lock = f
            .app()
            .join(format!(".git/refs/remotes/mirror/{DEFAULT}.lock"));
        assert!(!mirror_lock.exists());
        next_create_fetches(&f, "after");
    }

    /// An origin reached over "ssh" whose stand-in runs a setup script, then is held at a [`Gate`] like a
    /// [`HeldHook`], then fails. Dropping it releases the gate.
    struct HeldOrigin {
        entered: Entered,
        gate: Option<Gate>,
    }

    impl HeldOrigin {
        fn install(f: &GitFixture, setup: &str) -> Self {
            let tmp = f.tmp();
            let entered = tmp.join("origin-entered.fifo");
            mkfifo(&entered);
            let gate = Gate::new(tmp.join("origin-gate.fifo"));
            let ssh = tmp.join("held-ssh");
            crate::stand_in::install(
                &ssh,
                &format!(
                    "{setup}\nexec 3< '{}'\necho entered > '{}'\n{}\nexit 1\n",
                    gate.path.display(),
                    entered.display(),
                    Gate::WAIT
                ),
            );
            let app = f.app();
            f.git(&app, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
            f.git(&app, &["config", "ssh.variant", "simple"]);
            let url = "ssh://held.invalid/x.git";
            f.git(&app, &["remote", "set-url", "origin", url]);
            Self {
                entered: Entered::watch(&entered),
                gate: Some(gate),
            }
        }
    }

    impl HeldCreate for HeldOrigin {
        fn wait_entered(&self, limit: Duration) -> bool {
            self.entered.wait_entered(limit)
        }
    }

    impl Drop for HeldOrigin {
        fn drop(&mut self) {
            if let Some(gate) = self.gate.take() {
                gate.release();
            }
        }
    }

    /// Another process locks `origin/<default>` while the fetch runs (here the "ssh" serving it, which is then held
    /// until the cancel): that lock is not the fetch's, and stays.
    #[test]
    fn a_cancelled_fetch_leaves_a_lock_taken_meanwhile_alone() {
        let f = GitFixture::new();
        let lock = tracking_lock(&f);
        let held = HeldOrigin::install(&f, &format!("touch '{}'", lock.display()));
        let result = cancelled_while_held(&f, "foreign", held);
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert!(lock.exists());
    }

    /// Points origin at an "ssh" stand-in that runs `setup` (as another process acting while the fetch runs), then
    /// serves the fetch from the local origin. Origin moves first, so the fetch has an update to make.
    fn serve_origin_after(f: &GitFixture, setup: &str) {
        f.advance_origin();
        let app = f.app();
        // `$2` is `git-upload-pack '<path>'`. The variables git sets for its own repo are cleared, so `setup`'s git
        // calls and upload-pack find theirs.
        let ssh = f.tmp().join("serving-ssh");
        crate::stand_in::install(
            &ssh,
            &format!(
                "unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_PREFIX\n{setup}\n\
                 exec sh -c \"git ${{2#git-}}\"\n"
            ),
        );
        f.git(&app, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
        f.git(&app, &["config", "ssh.variant", "simple"]);
        let origin = f.tmp().join("origin.git");
        let url = format!("ssh://serving.invalid{}", origin.display());
        f.git(&app, &["remote", "set-url", "origin", &url]);
    }

    /// The lock of the ref the fetch of worktree `n` lands in.
    fn fetched_ref_lock(f: &GitFixture, n: &str) -> PathBuf {
        f.app().join(format!(".git/refs/shepherd/fetch/{n}.lock"))
    }

    /// Another process holds the lock of `origin/<default>` once the fetch is done (taken while it ran, by the "ssh"
    /// that serves it): moving `origin/<default>` fails at once whatever the user's lock timeout (with a short bound on
    /// that call, waiting on the lock would fail the create), the lock stays, and the worktree starts from the fetched
    /// commit all the same.
    #[test]
    fn a_foreign_lock_of_origin_default_is_left_alone_and_the_worktree_starts_from_the_fetch() {
        let f = GitFixture::new();
        f.git(&f.app(), &["config", "core.filesRefLockTimeout", "-1"]);
        let lock = tracking_lock(&f);
        serve_origin_after(&f, &format!("touch '{}'", lock.display()));
        let created = slow_port(&f)
            .create(&f.repo(), &name("blocked"), BaseSpec::OriginDefault)
            .unwrap();
        let fetched = BaseUsed::Origin {
            branch: BranchName::new(DEFAULT),
        };
        assert_eq!(created.base, fetched);
        assert_eq!(f.head(created.path.as_path()), f.origin_tip());
        assert!(lock.exists());
        assert_eq!(fetch_leftovers(&f), Vec::<String>::new());
    }

    /// Someone moves `origin/<default>` while the fetch runs (here the "ssh" serving it, as the user's own fetch might):
    /// moving it to what this fetch got would undo that, so it is left where they put it, and the worktree starts from
    /// the fetched commit all the same.
    /// `origin/<default>` missing before the create (`origin/HEAD` still names it) is made at the fetched commit.
    #[test]
    fn a_missing_origin_default_is_made_at_the_fetched_commit() {
        let f = GitFixture::new();
        f.delete_remote_tracking_default();
        f.advance_origin();
        let created = f
            .port
            .create(&f.repo(), &name("fresh"), BaseSpec::OriginDefault)
            .unwrap();
        let fetched = BaseUsed::Origin {
            branch: BranchName::new(DEFAULT),
        };
        assert_eq!(created.base, fetched);
        let tracking = format!("refs/remotes/origin/{DEFAULT}");
        assert_eq!(f.git(&f.app(), &["rev-parse", &tracking]), f.origin_tip());
    }

    #[test]
    fn moving_origin_default_never_undoes_a_move_made_meanwhile() {
        let f = GitFixture::new();
        let app = f.app();
        f.git(&app, &["checkout", "--quiet", "-b", "theirs"]);
        f.commit_in(&app);
        let theirs = f.git(&app, &["rev-parse", "HEAD"]);
        let tracking = format!("refs/remotes/origin/{DEFAULT}");
        let setup = format!("git -C '{}' update-ref {tracking} {theirs}", app.display());
        serve_origin_after(&f, &setup);
        let created = f
            .port
            .create(&f.repo(), &name("raced"), BaseSpec::OriginDefault)
            .unwrap();
        let fetched = BaseUsed::Origin {
            branch: BranchName::new(DEFAULT),
        };
        assert_eq!(created.base, fetched);
        assert_eq!(f.head(created.path.as_path()), f.origin_tip());
        assert_eq!(f.git(&app, &["rev-parse", &tracking]), theirs);
    }

    /// Another process takes the lock of the fetch's own ref while the fetch runs (here the "ssh" serving it): the
    /// fetch fails on it by itself, not killed, so that lock is not one it died holding, and stays.
    #[test]
    fn a_fetch_that_fails_on_a_lock_of_its_ref_leaves_that_lock_alone() {
        let f = GitFixture::new();
        let lock = fetched_ref_lock(&f, "locked");
        let dir = lock.parent().unwrap();
        let setup = format!("mkdir -p '{}'\ntouch '{}'", dir.display(), lock.display());
        serve_origin_after(&f, &setup);
        let created = f
            .port
            .create(&f.repo(), &name("locked"), BaseSpec::OriginDefault)
            .unwrap();
        assert!(
            matches!(created.base, BaseUsed::Stale { .. }),
            "{:?}",
            created.base
        );
        assert!(lock.exists());
    }

    /// A lock of the fetch's own ref there before the create (say, left by a host killed mid-fetch) is not one the
    /// fetch took, so a cancel leaves it.
    #[test]
    fn a_cancelled_fetch_leaves_a_lock_of_its_ref_there_before_alone() {
        let f = GitFixture::new();
        let lock = fetched_ref_lock(&f, "leftover");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, "").unwrap();
        let held = HeldOrigin::install(&f, "");
        let result = cancelled_while_held(&f, "leftover", held);
        assert_eq!(result, Err(WorktreeError::Cancelled));
        assert!(lock.exists());
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

    /// An HTTP origin on localhost that answers every request with `401`, so git asks for credentials. Each connection
    /// is answered on a thread of its own; the listener lives until the test process exits.
    fn origin_asking_for_credentials() -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x.git", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                        line.clear();
                    }
                    let _ = (&stream).write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                });
            }
        });
        url
    }

    /// The user's askpass helpers (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`, e.g. an editor's) would ask a person
    /// with no terminal to ask on, and with no cap on a create could wait forever. Git never runs them: the fetch fails
    /// at once and the create falls back to the last fetched origin default.
    #[test]
    fn a_credential_prompt_never_runs_the_users_askpass() {
        let f = GitFixture::new();
        let ran = f.tmp().join("askpass.ran");
        let askpass = f.tmp().join("askpass");
        crate::stand_in::install(
            &askpass,
            &format!("echo \"$@\" >> '{}'\necho secret\n", ran.display()),
        );
        let askpass_arg = askpass.to_str().unwrap();
        f.git(&f.app(), &["config", "core.askPass", askpass_arg]);
        let url = origin_asking_for_credentials();
        f.git(&f.app(), &["remote", "set-url", "origin", &url]);
        let mut vars = env(f.tmp().as_path());
        vars.push(("GIT_ASKPASS".into(), askpass.clone().into()));
        vars.push(("SSH_ASKPASS".into(), askpass.clone().into()));
        let port = GitWorktrees::new(f.root())
            .with_isolated_env(vars)
            .with_git_timeout(TEST_TIMEOUT)
            .with_fetch_timeout(TEST_TIMEOUT)
            .with_cleanup_timeout(TEST_TIMEOUT);
        let created = port
            .create(&f.repo(), &name("asked"), BaseSpec::OriginDefault)
            .unwrap();
        let BaseUsed::Stale { fetch_error, .. } = &created.base else {
            panic!("{:?}", created.base);
        };
        assert!(!ran.exists(), "askpass ran: {fetch_error}");
        assert!(!fetch_error.contains("timed out"), "{fetch_error}");
    }

    /// The background `sleep` keeps git's stdout and stderr open for an hour after git exits, so a create that returns
    /// within [`CREATE_LIMIT`] (a liveness bound, never a measurement) didn't wait for it.
    #[test]
    fn a_hook_that_leaves_a_background_process_does_not_hold_the_call() {
        let f = GitFixture::new();
        let pid_file = f.tmp().join("background.pid");
        hook(
            &f,
            "post-checkout",
            &format!(
                "{} &\necho $! > '{}'",
                marked_sleep(3600, &pid_file),
                pid_file.display()
            ),
        );
        let started = Instant::now();
        let result = f
            .port
            .create(&f.repo(), &name("bg"), BaseSpec::OriginDefault);
        let elapsed = started.elapsed();
        if let Ok(pid) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
        {
            kill_if_ours(pid, &pid_file);
        }
        assert!(result.is_ok(), "{result:?}");
        assert!(elapsed < CREATE_LIMIT, "took {elapsed:?}");
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
        crate::stand_in::install(&hook, &format!("touch '{}'\n", marker.display()));
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
