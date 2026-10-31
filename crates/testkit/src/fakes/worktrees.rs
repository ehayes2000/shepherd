//! An in-memory `Worktrees` that simulates git: a commit graph, an origin with branches, the clone's local and
//! remote-tracking branches, and its worktrees. Paths are only names: nothing touches the filesystem.
//!
//! A repo is added with [`FakeWorktrees::add_repo`], which models a fresh clone: origin has one commit on the
//! default branch, and the clone has the local default branch and `origin/<default>` at that commit. The simulation
//! methods (`commit`, `push`, `advance_origin`, ...) stand in for what the user and other people do to the repo.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.
//!
//! [`FakeWorktrees::hold_next`] makes the next call block until the test releases it, to stand in for a slow git call
//! (a fetch over the network) without timing anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub use super::hold::Hold;
use super::hold::{Gate, HoldSlot};
use shepherd_core::ports::worktrees::{managed_parts, normalize_lexically};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, CommitHash, Created, PatchId, RepoRef, RepoRoot, Worktree,
    WorktreeError, WorktreeName, WorktreePath, WorktreeStatus, Worktrees,
};

/// A commit in the simulated graph.
type Commit = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Head {
    Branch(String),
    Detached(Commit),
}

#[derive(Debug, Clone)]
struct Checkout {
    head: Head,
    dirty: bool,
}

#[derive(Debug)]
struct Repo {
    name: String,
    default: String,
    /// Branches on origin itself.
    origin: BTreeMap<String, Commit>,
    /// `origin/*` as of the last fetch.
    tracking: BTreeMap<String, Commit>,
    local: BTreeMap<String, Commit>,
    /// Local branch → the origin branch it tracks.
    upstream: BTreeMap<String, String>,
    /// Every checkout, the main one included.
    checkouts: BTreeMap<PathBuf, Checkout>,
    main: PathBuf,
    offline: Option<String>,
    /// Whether the clone has `origin/HEAD`. Origin always knows its HEAD; a missing one is repaired on demand when
    /// online.
    head_known: bool,
}

#[derive(Debug, Default)]
struct State {
    /// `parents[c]` are commit `c`'s parents.
    parents: Vec<Vec<Commit>>,
    /// `changes[c]` is the change commit `c` makes, which names its patch-id: its own number, or for a commit a rebase
    /// made, the change of the commit it was made from. `None` for a merge or a commit that changes nothing, which have
    /// no patch-id.
    changes: Vec<Option<usize>>,
    repos: Vec<Repo>,
    /// Directories inside checkouts, from [`FakeWorktrees::add_dir`]. A checkout itself always exists.
    dirs: BTreeSet<PathBuf>,
    fail_next: Option<WorktreeError>,
    /// Checkouts whose every `status` fails with this, from [`FakeWorktrees::fail_status`].
    failing_status: BTreeMap<PathBuf, WorktreeError>,
}

/// A create's repo (as the caller named it) and worktree name: what [`Worktrees::cancel_create`] names.
type CreateKey = (PathBuf, String);

/// Where a create [`Worktrees::cancel_create`] names is.
#[derive(Debug)]
enum CreateState {
    /// Cancelled before it started: the next create of it is cancelled.
    CancelledEarly,
    /// Running, held at `gate` if it was held.
    Running {
        cancelled: bool,
        gate: Option<Arc<Gate>>,
    },
}

#[derive(Debug, Clone)]
pub struct FakeWorktrees {
    root: PathBuf,
    state: Arc<Mutex<State>>,
    /// The gate the next call waits at, if [`FakeWorktrees::hold_next`] armed one.
    hold: HoldSlot,
    /// Creates running or cancelled early. Kept apart from `state`, so a held create holds no lock.
    creates: Arc<Mutex<BTreeMap<CreateKey, CreateState>>>,
}

/// Forgets a create once it returns.
struct Running<'a> {
    creates: &'a Mutex<BTreeMap<CreateKey, CreateState>>,
    key: CreateKey,
}

impl Running<'_> {
    fn cancelled(&self) -> bool {
        matches!(
            lock(self.creates).get(&self.key),
            Some(CreateState::Running {
                cancelled: true,
                ..
            })
        )
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        lock(self.creates).remove(&self.key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// How commit `c` is named: 40 hex digits, like a real commit hash, lowest digit first, so that a test's commits differ
/// in their first digits and a short prefix names one, as with a real hash.
fn commit_name(c: Commit) -> String {
    format!("{c:040x}").chars().rev().collect()
}

fn commit_hash(c: Commit) -> CommitHash {
    CommitHash::parse(&commit_name(c)).unwrap_or_else(|e| unreachable!("{e}"))
}

/// The patch-id of `change`: 40 hex digits ending with `f`, which no commit of a test is named.
fn patch_id(change: usize) -> PatchId {
    let digits: String = format!("{change:039x}").chars().rev().collect();
    PatchId::parse(&format!("{digits}f")).unwrap_or_else(|e| unreachable!("{e}"))
}

impl FakeWorktrees {
    /// A fake managing worktrees under `root`, with no repos.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: Arc::default(),
            hold: HoldSlot::default(),
            creates: Arc::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A fresh clone at `path` of an origin whose default branch is `default`. The repo name is `path`'s last
    /// component.
    pub fn add_repo(&self, path: impl Into<PathBuf>, default: &str) -> RepoRef {
        let path = path.into();
        let mut s = self.lock();
        let c = s.new_commit(vec![]);
        let one = BTreeMap::from([(default.to_owned(), c)]);
        s.repos.push(Repo {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            default: default.to_owned(),
            origin: one.clone(),
            tracking: one.clone(),
            local: one,
            upstream: BTreeMap::from([(default.to_owned(), default.to_owned())]),
            checkouts: BTreeMap::from([(
                path.clone(),
                Checkout {
                    head: Head::Branch(default.to_owned()),
                    dirty: false,
                },
            )]),
            main: path.clone(),
            offline: None,
            head_known: true,
        });
        RepoRef::new(path)
    }

    /// Makes `path`, a directory inside a checkout, and its parents exist. Only [`Worktrees::repo_root`] asks: it
    /// treats any other path below a checkout as missing.
    pub fn add_dir(&self, path: impl Into<PathBuf>) {
        self.lock().dirs.insert(path.into());
    }

    /// The next port call fails with `error` and changes nothing.
    pub fn fail_next(&self, error: WorktreeError) {
        self.lock().fail_next = Some(error);
    }

    /// Every `status` of the checkout at `path` fails with `error` from now on, while `list_managed` still lists it: as
    /// git does for a worktree whose `.git` file is gone.
    pub fn fail_status(&self, path: impl Into<PathBuf>, error: WorktreeError) {
        self.lock().failing_status.insert(path.into(), error);
    }

    /// The next port call blocks, before it does anything, until the returned [`Hold`] is released or dropped. Calls
    /// after it are not held. A call held for longer than a generous liveness bound panics.
    pub fn hold_next(&self) -> Hold {
        self.hold.arm()
    }

    /// How many creates are running right now (started and not yet returned), held ones included.
    pub fn creates_running(&self) -> usize {
        lock(&self.creates)
            .values()
            .filter(|c| matches!(c, CreateState::Running { .. }))
            .count()
    }

    /// Commits on the branch (or detached HEAD) checked out at `path`. The checkout becomes clean.
    pub fn commit(&self, path: &Path) {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let parent = s.repos[r].resolve(&head);
        let c = s.new_commit(parent.into_iter().collect());
        let repo = &mut s.repos[r];
        match head {
            Head::Branch(b) => {
                repo.local.insert(b, c);
            }
            Head::Detached(_) => {}
        }
        if let Some(co) = repo.checkouts.get_mut(path) {
            co.dirty = false;
            if let Head::Detached(_) = co.head {
                co.head = Head::Detached(c);
            }
        }
    }

    /// `git commit --allow-empty` on the branch checked out at `path`: a commit that changes nothing.
    pub fn commit_empty(&self, path: &Path) {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let Head::Branch(b) = head else {
            panic!("an empty commit on a detached HEAD")
        };
        let tip = s.repos[r].local[&b];
        let c = s.copy_commit(vec![tip], None);
        s.repos[r].local.insert(b, c);
    }

    /// Marks the checkout at `path` as having (or not having) uncommitted changes.
    pub fn set_dirty(&self, path: &Path, dirty: bool) {
        let mut s = self.lock();
        let r = s
            .repo_of(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        if let Some(co) = s.repos[r].checkouts.get_mut(path) {
            co.dirty = dirty;
        }
    }

    /// `git push -u origin <branch>` from `path`: origin gets the branch, which becomes its upstream.
    pub fn push(&self, path: &Path) {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let Head::Branch(b) = head else {
            panic!("push from a detached HEAD")
        };
        let repo = &mut s.repos[r];
        let tip = repo.local[&b];
        repo.origin.insert(b.clone(), tip);
        repo.tracking.insert(b.clone(), tip);
        repo.upstream.insert(b.clone(), b);
    }

    /// Origin's default branch moves to the HEAD of `path` (as a merge would), without the clone fetching it.
    pub fn land_on_origin(&self, path: &Path) {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let repo = &mut s.repos[r];
        let tip = repo
            .resolve(&head)
            .unwrap_or_else(|| panic!("nothing to land"));
        let default = repo.default.clone();
        repo.origin.insert(default, tip);
    }

    /// Someone else pushes a commit to origin's default branch. The clone has not fetched it.
    pub fn advance_origin(&self, repo: &RepoRef) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        let default = s.repos[r].default.clone();
        let parent = s.repos[r].origin.get(&default).copied();
        let c = s.new_commit(parent.into_iter().collect());
        s.repos[r].origin.insert(default, c);
    }

    /// `git fetch origin`: the clone's `origin/*` catch up with origin. Works even when offline, so tests can set up
    /// state; only fetches made by the port honor [`FakeWorktrees::set_offline`].
    pub fn fetch(&self, repo: &RepoRef) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        let repo = &mut s.repos[r];
        repo.tracking = repo.origin.clone();
    }

    /// Makes every fetch the port does fail with `error` (`Some`), or succeed again (`None`).
    pub fn set_offline(&self, repo: &RepoRef, error: Option<&str>) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        s.repos[r].offline = error.map(str::to_owned);
    }

    /// Deletes the clone's `origin/HEAD`.
    pub fn forget_origin_head(&self, repo: &RepoRef) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        s.repos[r].head_known = false;
    }

    /// Deletes the clone's remote-tracking `origin/<branch>`.
    pub fn delete_tracking(&self, repo: &RepoRef, branch: &str) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        s.repos[r].tracking.remove(branch);
    }

    /// Deletes a local branch. The main checkout is detached first if it is on it.
    pub fn delete_branch(&self, repo: &RepoRef, branch: &str) {
        let mut s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        let repo = &mut s.repos[r];
        let Some(tip) = repo.local.remove(branch) else {
            return;
        };
        repo.upstream.remove(branch);
        for co in repo.checkouts.values_mut() {
            if co.head == Head::Branch(branch.to_owned()) {
                co.head = Head::Detached(tip);
            }
        }
    }

    /// `git rebase origin/<default>` in `path`: the commits of its branch that `origin/<default>` doesn't have are made
    /// again on top of it, with the same changes.
    pub fn rebase_onto_origin_default(&self, path: &Path) {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let Head::Branch(b) = head else {
            panic!("rebase of a detached HEAD")
        };
        let repo = &s.repos[r];
        let (tip, onto) = (repo.local[&b], repo.tracking[&repo.default]);
        let mut at = onto;
        for c in s.since(tip, onto) {
            let change = s.changes[c];
            at = s.copy_commit(vec![at], change);
        }
        s.repos[r].local.insert(b, at);
    }

    /// `git merge --allow-unrelated-histories` of a new root commit into the branch checked out at `path`. Returns the
    /// new root's name, as [`FakeWorktrees::head`] names commits.
    pub fn merge_unrelated_root(&self, path: &Path) -> String {
        let mut s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        let Head::Branch(b) = head else {
            panic!("merge into a detached HEAD")
        };
        let tip = s.repos[r].local[&b];
        let root = s.new_commit(Vec::new());
        let merge = s.new_commit(vec![tip, root]);
        s.repos[r].local.insert(b, merge);
        commit_name(root)
    }

    /// An opaque name for the commit checked out at `path`.
    pub fn head(&self, path: &Path) -> String {
        let s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        commit_name(s.repos[r].resolve(&head).unwrap_or(usize::MAX))
    }

    /// An opaque name for the commit at the tip of origin's `branch`, or the clone's local `branch`.
    pub fn tip(&self, repo: &RepoRef, branch: &str, on_origin: bool) -> String {
        let s = self.lock();
        let r = s
            .repo_of(repo.as_path())
            .unwrap_or_else(|| panic!("no repo at {repo}"));
        let refs = if on_origin {
            &s.repos[r].origin
        } else {
            &s.repos[r].local
        };
        commit_name(refs.get(branch).copied().unwrap_or(usize::MAX))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every port call starts here: waits if held, then fails if a failure was injected.
    fn take_failure(&self) -> Result<MutexGuard<'_, State>, WorktreeError> {
        self.hold.wait_if_held("FakeWorktrees");
        self.injected_failure()
    }

    fn injected_failure(&self) -> Result<MutexGuard<'_, State>, WorktreeError> {
        let mut s = self.lock();
        match s.fail_next.take() {
            Some(e) => Err(e),
            None => Ok(s),
        }
    }
}

impl State {
    /// A new commit with `parents`, making a change of its own unless it is a merge.
    fn new_commit(&mut self, parents: Vec<Commit>) -> Commit {
        let c = self.parents.len();
        let change = (parents.len() < 2).then_some(c);
        self.copy_commit(parents, change)
    }

    /// A new commit with `parents` that makes `change`.
    fn copy_commit(&mut self, parents: Vec<Commit>, change: Option<usize>) -> Commit {
        self.parents.push(parents);
        self.changes.push(change);
        self.parents.len() - 1
    }

    /// The commit `hash` names, if there is one.
    fn commit_named(&self, hash: &CommitHash) -> Option<Commit> {
        let digits: String = hash.as_str().chars().rev().collect();
        let c = usize::from_str_radix(&digits, 16).ok()?;
        (c < self.parents.len()).then_some(c)
    }

    /// The commit `rev` names in repo `r` with `head` checked out: `HEAD`, a local branch, or a full or unique
    /// abbreviated (at least 4 digits) commit name.
    fn resolve_rev(&self, r: usize, head: &Head, rev: &str) -> Option<Commit> {
        let repo = &self.repos[r];
        if rev == "HEAD" {
            return repo.resolve(head);
        }
        if let Some(&c) = repo.local.get(rev) {
            return Some(c);
        }
        if rev.len() < 4 || !rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut named = (0..self.parents.len()).filter(|&c| commit_name(c).starts_with(rev));
        match (named.next(), named.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }

    /// Commits reachable from `tip` but not from `base`, oldest first (a commit's parents are older than it).
    fn since(&self, tip: Commit, base: Commit) -> Vec<Commit> {
        let base = self.reachable(base);
        self.reachable(tip).difference(&base).copied().collect()
    }

    /// The repo and checkout that `path` is in (the checkout itself or any path under it), as git finds them.
    fn locate(&self, path: &Path) -> Option<(usize, PathBuf)> {
        self.repos
            .iter()
            .enumerate()
            .flat_map(|(r, repo)| repo.checkouts.keys().map(move |c| (r, c)))
            .filter(|(_, c)| path.starts_with(c))
            .max_by_key(|(_, c)| c.components().count())
            .map(|(r, c)| (r, c.clone()))
    }

    fn repo_of(&self, path: &Path) -> Option<usize> {
        self.locate(path).map(|(r, _)| r)
    }

    fn checkout(&self, path: &Path) -> Option<(usize, Head)> {
        let (r, top) = self.locate(path)?;
        Some((r, self.repos[r].checkouts[&top].head.clone()))
    }

    fn reachable(&self, from: Commit) -> BTreeSet<Commit> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![from];
        while let Some(c) = todo.pop() {
            if seen.insert(c) {
                todo.extend(&self.parents[c]);
            }
        }
        seen
    }

    /// Commits reachable from `tip` but not from `base`.
    fn ahead(&self, tip: Commit, base: Option<Commit>) -> usize {
        let base = base.map(|b| self.reachable(b)).unwrap_or_default();
        self.reachable(tip).difference(&base).count()
    }

    fn not_a_repo(path: &Path) -> WorktreeError {
        WorktreeError::NotARepo(path.to_owned())
    }
}

impl Repo {
    /// The default branch, asking origin for its HEAD if the clone has lost it.
    fn default_branch(&mut self) -> Result<String, WorktreeError> {
        if !self.head_known {
            if self.offline.is_some() {
                return Err(WorktreeError::NoDefaultBranch(self.main.clone()));
            }
            self.head_known = true;
        }
        Ok(self.default.clone())
    }

    fn resolve(&self, head: &Head) -> Option<Commit> {
        match head {
            Head::Branch(b) => self.local.get(b).copied(),
            Head::Detached(c) => Some(*c),
        }
    }
}

impl Worktrees for FakeWorktrees {
    /// Every fake repo has a main checkout, which names it. Below a checkout, only directories made with
    /// [`FakeWorktrees::add_dir`] exist.
    fn repo_root(&self, path: &Path) -> Result<PathBuf, WorktreeError> {
        let s = self.take_failure()?;
        let (r, top) = s.locate(path).ok_or_else(|| State::not_a_repo(path))?;
        let exists = top == path || s.dirs.iter().any(|d| d.starts_with(path));
        if !exists {
            return Err(State::not_a_repo(path));
        }
        Ok(s.repos[r].main.clone())
    }

    fn default_branch(&self, repo: &RepoRef) -> Result<BranchName, WorktreeError> {
        let mut s = self.take_failure()?;
        let r = s
            .repo_of(repo.as_path())
            .ok_or_else(|| State::not_a_repo(repo.as_path()))?;
        s.repos[r].default_branch().map(BranchName::new)
    }

    fn create(
        &self,
        repo: &RepoRef,
        name: &WorktreeName,
        base: BaseSpec,
    ) -> Result<Created, WorktreeError> {
        let BaseSpec::OriginDefault = base;
        let key = (repo.as_path().to_owned(), name.as_str().to_owned());
        // Forgotten when this returns, so a cancel after that waits for the next create of it.
        let _running = {
            let mut creates = lock(&self.creates);
            match creates.get(&key) {
                Some(CreateState::CancelledEarly) => {
                    creates.remove(&key);
                    return Err(WorktreeError::Cancelled);
                }
                Some(CreateState::Running { .. }) => {
                    return Err(WorktreeError::Other(format!(
                        "{name} is already being created"
                    )));
                }
                None => {}
            }
            let gate = self.hold.take();
            creates.insert(
                key.clone(),
                CreateState::Running {
                    cancelled: false,
                    gate: gate.clone(),
                },
            );
            drop(creates);
            let running = Running {
                creates: &self.creates,
                key,
            };
            if let Some(gate) = gate {
                gate.wait("FakeWorktrees");
            }
            if running.cancelled() {
                return Err(WorktreeError::Cancelled);
            }
            running
        };
        let mut s = self.injected_failure()?;
        let r = s
            .repo_of(repo.as_path())
            .ok_or_else(|| State::not_a_repo(repo.as_path()))?;
        let repo_dir = self.root.join(&s.repos[r].name);
        let taken = s.repos.iter().enumerate().any(|(i, other)| {
            i != r
                && other
                    .checkouts
                    .keys()
                    .any(|p| p.parent() == Some(repo_dir.as_path()))
        });
        if taken {
            return Err(WorktreeError::RepoNameTaken { path: repo_dir });
        }
        let rp = &mut s.repos[r];
        let default = BranchName::new(rp.default_branch()?);
        let branch = name.branch();
        if rp.local.contains_key(branch.as_str()) {
            return Err(WorktreeError::BranchExists(branch));
        }
        let path = repo_dir.join(name.as_str());
        if rp.checkouts.contains_key(&path) {
            return Err(WorktreeError::Other(format!(
                "{} already exists",
                path.display()
            )));
        }
        let (start, base) = match rp.offline.clone() {
            None => {
                rp.tracking = rp.origin.clone();
                let start = rp.tracking[default.as_str()];
                (start, BaseUsed::Origin { branch: default })
            }
            Some(fetch_error) => {
                if let Some(&start) = rp.tracking.get(default.as_str()) {
                    let base = BaseUsed::Stale {
                        branch: default,
                        fetch_error,
                    };
                    (start, base)
                } else if let Some(&start) = rp.local.get(default.as_str()) {
                    let base = BaseUsed::Local {
                        branch: default,
                        fetch_error,
                    };
                    (start, base)
                } else {
                    return Err(WorktreeError::FetchFailed(fetch_error));
                }
            }
        };
        rp.local.insert(branch.as_str().to_owned(), start);
        rp.checkouts.insert(
            path.clone(),
            Checkout {
                head: Head::Branch(branch.as_str().to_owned()),
                dirty: false,
            },
        );
        Ok(Created {
            path: WorktreePath::new(path),
            branch,
            base,
            base_commit: Some(commit_hash(start)),
        })
    }

    /// A held create stops waiting at once. Once a create is past its hold it can't be cancelled: the rest of it is
    /// instant.
    fn cancel_create(&self, repo: &RepoRef, name: &WorktreeName) {
        let key = (repo.as_path().to_owned(), name.as_str().to_owned());
        let mut creates = lock(&self.creates);
        match creates.get_mut(&key) {
            Some(CreateState::Running { cancelled, gate }) => {
                *cancelled = true;
                if let Some(gate) = gate {
                    gate.cancel();
                }
            }
            Some(CreateState::CancelledEarly) => {}
            None => {
                creates.insert(key, CreateState::CancelledEarly);
            }
        }
    }

    fn list_managed(&self, repo: &RepoRef) -> Result<Vec<Worktree>, WorktreeError> {
        let s = self.take_failure()?;
        let r = s
            .repo_of(repo.as_path())
            .ok_or_else(|| State::not_a_repo(repo.as_path()))?;
        let rp = &s.repos[r];
        Ok(rp
            .checkouts
            .iter()
            .filter(|(path, _)| {
                managed_parts(&self.root, path).is_some_and(|(repo, _)| repo == rp.name)
            })
            .map(|(path, co)| Worktree {
                path: WorktreePath::new(path),
                branch: match &co.head {
                    Head::Branch(b) => Some(BranchName::new(b)),
                    Head::Detached(_) => None,
                },
            })
            .collect())
    }

    fn status(&self, path: &WorktreePath) -> Result<WorktreeStatus, WorktreeError> {
        let mut s = self.take_failure()?;
        let path = path.as_path();
        if let Some(error) = s.failing_status.get(path) {
            return Err(error.clone());
        }
        let (r, head) = s.checkout(path).ok_or_else(|| State::not_a_repo(path))?;
        s.repos[r].default_branch()?;
        let rp = &s.repos[r];
        let tip = rp
            .resolve(&head)
            .ok_or_else(|| WorktreeError::Other("HEAD has no commits".into()))?;
        let default = rp
            .tracking
            .get(&rp.default)
            .copied()
            .ok_or_else(|| WorktreeError::NoDefaultBranch(rp.main.clone()))?;
        let upstream_tip = match &head {
            Head::Branch(b) => rp.upstream.get(b).and_then(|u| rp.tracking.get(u)).copied(),
            Head::Detached(_) => None,
        };
        let ahead = s.ahead(tip, Some(default));
        Ok(WorktreeStatus {
            dirty: s
                .locate(path)
                .is_some_and(|(_, top)| rp.checkouts[&top].dirty),
            ahead: u32::try_from(ahead).unwrap_or(u32::MAX),
            pushed: upstream_tip.is_some_and(|u| s.ahead(tip, Some(u)) == 0),
            merged_into_default: ahead == 0,
        })
    }

    fn remove(&self, path: &WorktreePath, force: bool) -> Result<(), WorktreeError> {
        let mut s = self.take_failure()?;
        let outside = || WorktreeError::OutsideManagedRoot(path.as_path().to_owned());
        let normal = normalize_lexically(path.as_path()).ok_or_else(outside)?;
        let root = normalize_lexically(&self.root).ok_or_else(outside)?;
        managed_parts(&root, &normal).ok_or_else(outside)?;
        let r = s
            .repos
            .iter()
            .position(|r| r.checkouts.contains_key(&normal))
            .ok_or_else(|| State::not_a_repo(&normal))?;
        let rp = &mut s.repos[r];
        if rp.main == normal {
            return Err(WorktreeError::Other(format!(
                "{} is a main checkout",
                normal.display()
            )));
        }
        if rp.checkouts[&normal].dirty && !force {
            return Err(WorktreeError::Dirty(path.clone()));
        }
        rp.checkouts.remove(&normal);
        Ok(())
    }

    /// A merge, or a commit [`FakeWorktrees::commit_empty`] made, has no patch-id; every other commit has one.
    fn commits_since(
        &self,
        path: &WorktreePath,
        base: &CommitHash,
    ) -> Result<Vec<(CommitHash, Option<PatchId>)>, WorktreeError> {
        let s = self.take_failure()?;
        let path = path.as_path();
        let (r, head) = s.checkout(path).ok_or_else(|| State::not_a_repo(path))?;
        let tip = s.repos[r]
            .resolve(&head)
            .ok_or_else(|| WorktreeError::Other("HEAD has no commits".into()))?;
        let base = s
            .commit_named(base)
            .ok_or_else(|| WorktreeError::Other(format!("unknown revision {base}")))?;
        Ok(s.since(tip, base)
            .into_iter()
            .map(|c| (commit_hash(c), s.changes[c].map(patch_id)))
            .collect())
    }

    /// Every repo shares one commit graph, so any repo has every commit; a name past the graph is unknown.
    fn patch_id(
        &self,
        repo: &RepoRef,
        commit: &CommitHash,
    ) -> Result<Option<PatchId>, WorktreeError> {
        let s = self.take_failure()?;
        let path = repo.as_path();
        s.checkout(path).ok_or_else(|| State::not_a_repo(path))?;
        let c = s
            .commit_named(commit)
            .ok_or_else(|| WorktreeError::Other(format!("bad object {commit}")))?;
        Ok(s.changes[c].map(patch_id))
    }

    fn resolve_commit(
        &self,
        repo: &RepoRef,
        rev: &str,
    ) -> Result<Option<CommitHash>, WorktreeError> {
        let s = self.take_failure()?;
        let path = repo.as_path();
        let (r, head) = s.checkout(path).ok_or_else(|| State::not_a_repo(path))?;
        Ok(s.resolve_rev(r, &head, rev).map(commit_hash))
    }

    /// Below a checkout, only directories made with [`FakeWorktrees::add_dir`] exist, as for `repo_root`.
    fn root_commit(&self, repo: &RepoRef) -> Result<RepoRoot, WorktreeError> {
        let s = self.take_failure()?;
        let path = repo.as_path();
        let (r, top) = s.locate(path).ok_or_else(|| State::not_a_repo(path))?;
        if top != path && !s.dirs.iter().any(|d| d.starts_with(path)) {
            return Err(State::not_a_repo(path));
        }
        let rp = &s.repos[r];
        let tip = rp
            .resolve(&rp.checkouts[&top].head)
            .ok_or_else(|| WorktreeError::Other("HEAD has no commits".into()))?;
        let root = s
            .reachable(tip)
            .into_iter()
            .filter(|&c| s.parents[c].is_empty())
            .map(commit_hash)
            .min()
            .ok_or_else(|| WorktreeError::Other("the history has no root".into()))?;
        Ok(RepoRoot::new(root))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::contracts::worktrees::{
        DEFAULT, HeldCreate, NESTED, REPO_NAME, SUBDIR, WorktreesFixture,
    };

    struct FakeFixture {
        fake: FakeWorktrees,
        repo: RepoRef,
    }

    impl FakeFixture {
        fn new() -> Self {
            let fake = FakeWorktrees::new("/fake/root");
            let repo = fake.add_repo(Path::new("/fake").join(REPO_NAME), DEFAULT);
            // Only the deepest directory: its parent exists through it.
            fake.add_dir(repo.as_path().join(SUBDIR).join(NESTED));
            Self { fake, repo }
        }
    }

    impl WorktreesFixture for FakeFixture {
        type Port = FakeWorktrees;

        fn port(&self) -> &FakeWorktrees {
            &self.fake
        }

        fn root(&self) -> PathBuf {
            self.fake.root().to_owned()
        }

        fn hold_next_create(&self) -> Box<dyn HeldCreate + '_> {
            Box::new(self.fake.hold_next())
        }

        fn repo(&self) -> RepoRef {
            self.repo.clone()
        }

        fn not_a_repo(&self) -> RepoRef {
            RepoRef::new("/fake/not-a-repo")
        }

        fn head(&self, path: &Path) -> String {
            self.fake.head(path)
        }

        fn origin_tip(&self) -> String {
            self.fake.tip(&self.repo, DEFAULT, true)
        }

        fn local_default_tip(&self) -> String {
            self.fake.tip(&self.repo, DEFAULT, false)
        }

        fn advance_origin(&self) {
            self.fake.advance_origin(&self.repo);
        }

        fn commit(&self, path: &WorktreePath) {
            self.fake.commit(path.as_path());
        }

        fn make_dirty(&self, path: &WorktreePath) {
            self.fake.set_dirty(path.as_path(), true);
        }

        fn push(&self, path: &WorktreePath) {
            self.fake.push(path.as_path());
        }

        fn land_on_origin_default(&self, path: &WorktreePath) {
            self.fake.land_on_origin(path.as_path());
            self.fake.fetch(&self.repo);
        }

        fn go_offline(&self) {
            self.fake
                .set_offline(&self.repo, Some("could not resolve host"));
        }

        fn hang_origin(&self, _timeout: Duration) -> FakeWorktrees {
            self.fake.set_offline(&self.repo, Some("fetch timed out"));
            self.fake.clone()
        }

        fn delete_local_default(&self) {
            self.fake.delete_branch(&self.repo, DEFAULT);
        }

        fn fetch(&self) {
            self.fake.fetch(&self.repo);
        }

        fn delete_remote_tracking_default(&self) {
            self.fake.delete_tracking(&self.repo, DEFAULT);
        }

        fn rebase_onto_origin_default(&self, path: &WorktreePath) {
            self.fake.fetch(&self.repo);
            self.fake.rebase_onto_origin_default(path.as_path());
        }

        fn merge_unrelated_root(&self, path: &WorktreePath) -> String {
            self.fake.merge_unrelated_root(path.as_path())
        }

        fn commit_empty(&self, path: &WorktreePath) {
            self.fake.commit_empty(path.as_path());
        }

        fn forget_origin_head(&self) {
            self.fake.forget_origin_head(&self.repo);
        }

        fn same_named_repo(&self) -> RepoRef {
            self.fake
                .add_repo(Path::new("/fake/other").join(REPO_NAME), DEFAULT)
        }

        fn resolved(&self, path: &Path) -> PathBuf {
            path.to_owned()
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::worktrees_contract!(FakeFixture::new());
    }

    #[test]
    fn fail_next_fails_one_call_and_changes_nothing() {
        let f = FakeFixture::new();
        f.fake
            .fail_next(WorktreeError::Timeout("worktree add".into()));
        let n = WorktreeName::new("x").unwrap();
        assert_eq!(
            f.fake.create(&f.repo, &n, BaseSpec::OriginDefault),
            Err(WorktreeError::Timeout("worktree add".into()))
        );
        assert!(f.fake.list_managed(&f.repo).unwrap().is_empty());
        assert!(f.fake.create(&f.repo, &n, BaseSpec::OriginDefault).is_ok());
    }

    #[test]
    fn a_held_call_waits_until_released_and_only_one_call_is_held() {
        let f = FakeFixture::new();
        let hold = f.fake.hold_next();
        let fake = f.fake.clone();
        let repo = f.repo.clone();
        let call = std::thread::spawn(move || fake.default_branch(&repo));
        assert!(hold.wait_entered(Duration::from_secs(10)));
        // Blocked in the call: the next call is not held, and the held one hasn't returned.
        assert_eq!(f.fake.default_branch(&f.repo), Ok(BranchName::new(DEFAULT)));
        assert!(!call.is_finished());
        hold.release();
        assert_eq!(call.join().unwrap(), Ok(BranchName::new(DEFAULT)));
    }

    #[test]
    fn deleting_a_worktrees_branch_detaches_it() {
        let f = FakeFixture::new();
        let n = WorktreeName::new("x").unwrap();
        let created = f.fake.create(&f.repo, &n, BaseSpec::OriginDefault).unwrap();
        f.fake.delete_branch(&f.repo, "x");
        let listed = f.fake.list_managed(&f.repo).unwrap();
        assert_eq!(
            listed,
            vec![Worktree {
                path: created.path.clone(),
                branch: None
            }]
        );
        assert!(!f.fake.status(&created.path).unwrap().pushed);
    }
}
