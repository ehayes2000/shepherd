//! `Worktrees` over the `git` CLI. Every call runs `git -C <dir> ...` and parses only porcelain or plumbing output.
//!
//! Each git process runs in its own process group with stdin closed and `GIT_TERMINAL_PROMPT=0`, so it can never wait
//! on a prompt. One that runs past its timeout is killed with its whole group (an ssh child included).

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

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

/// Variables that would point git at some other repo than the `-C` directory, e.g. when shepherd itself runs from a
/// git hook.
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

pub struct GitWorktrees {
    root: PathBuf,
    env: Vec<(OsString, OsString)>,
    git_timeout: Duration,
    fetch_timeout: Duration,
    /// Serializes fetch + `worktree add`: concurrent fetches into one repo race on ref locks.
    create_lock: Mutex<()>,
}

impl GitWorktrees {
    /// Manages worktrees under `root` (`~/.shepherd/worktrees` in production).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            env: Vec::new(),
            git_timeout: GIT_TIMEOUT,
            fetch_timeout: FETCH_TIMEOUT,
            create_lock: Mutex::new(()),
        }
    }

    /// Extra environment for every git call, on top of the inherited one (tests use it to isolate git).
    pub fn with_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.env = env;
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

    fn git<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<String, Failure> {
        self.git_within(dir, args, self.git_timeout)
    }

    fn git_within<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Duration,
    ) -> Result<String, Failure> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for var in REPO_ENV {
            command.env_remove(var);
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        let mut child = command.spawn().map_err(|e| Failure::Io(e.to_string()))?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let waited = child.wait_timeout(timeout);
        if !matches!(waited, Ok(Some(_))) {
            kill_group(child.id());
            let _ = child.wait();
        }
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        match waited {
            Err(e) => Err(Failure::Io(e.to_string())),
            Ok(None) => Err(Failure::Timeout),
            Ok(Some(status)) if status.success() => {
                String::from_utf8(stdout).map_err(|e| Failure::Io(e.to_string()))
            }
            Ok(Some(status)) => Err(Failure::Exit {
                code: status.code(),
                stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
            }),
        }
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
        let common = self.common_dir(dir)?;
        repo_name_from_common_dir(&common).ok_or_else(|| {
            WorktreeError::Other(format!("can't name the repo at {}", common.display()))
        })
    }

    /// `origin/HEAD`'s target, which must exist. Assumes `dir` is in a repo.
    fn default_of(&self, dir: &Path) -> Result<String, WorktreeError> {
        let none = || WorktreeError::NoDefaultBranch(dir.to_owned());
        let target = match self.git(
            dir,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        ) {
            Ok(out) => out,
            Err(Failure::Exit { .. }) => return Err(none()),
            Err(f) => return Err(f.into_error("symbolic-ref")),
        };
        let target = target.trim_end_matches('\n');
        let branch = target
            .strip_prefix("refs/remotes/origin/")
            .ok_or_else(none)?;
        if branch.is_empty() || !self.ref_exists(dir, target)? {
            return Err(none());
        }
        Ok(branch.to_owned())
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
            .git(dir, &["status", "--porcelain=v2", "--branch", "-z"])
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
        let repo_name = self.repo_name(dir)?;
        let default = self.default_of(dir)?;
        let branch = name.branch();
        let path = self.root.join(&repo_name).join(name.as_str());

        let _guard = self
            .create_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
            &["fetch", "--quiet", "origin", default.as_str()],
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
                let local = format!("refs/heads/{default}");
                if !self.ref_exists(dir, &local)? {
                    return Err(WorktreeError::FetchFailed(fetch_error));
                }
                (
                    local,
                    BaseUsed::LocalFallback {
                        branch: BranchName::new(&default),
                        fetch_error,
                    },
                )
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorktreeError::Other(format!("can't create {}: {e}", parent.display()))
            })?;
        }
        let args: [&OsStr; 8] = [
            "worktree".as_ref(),
            "add".as_ref(),
            "--quiet".as_ref(),
            "--no-track".as_ref(),
            "-b".as_ref(),
            branch.as_str().as_ref(),
            path.as_os_str(),
            start.as_ref(),
        ];
        self.git(dir, &args)
            .map_err(|f| f.into_error("worktree add"))?;
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
        let porcelain = self.porcelain_status(dir)?;
        let range = format!("refs/remotes/origin/{default}..HEAD");
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

/// Reads a pipe to the end on its own thread, so a chatty child can't block on a full pipe.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscall. The child is not reaped yet, so its group still exists.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use shepherd_core::ports::{RepoRef, WorktreePath};
    use shepherd_testkit::contracts::worktrees::{
        DEFAULT, FsWorktreesFixture, REPO_NAME, WorktreesFixture,
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

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env_clear()
                .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
                .envs(env(self.dir.path()))
                .output()
                .unwrap();
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
            .with_env(env(tmp))
            .with_git_timeout(TEST_TIMEOUT)
            .with_fetch_timeout(fetch_timeout)
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
}
