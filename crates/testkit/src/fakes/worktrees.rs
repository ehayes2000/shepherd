//! An in-memory `Worktrees` that simulates git: a commit graph, an origin with branches, the clone's local and
//! remote-tracking branches, and its worktrees. Paths are only names: nothing touches the filesystem.
//!
//! A repo is added with [`FakeWorktrees::add_repo`], which models a fresh clone: origin has one commit on the
//! default branch, and the clone has the local default branch and `origin/<default>` at that commit. The simulation
//! methods (`commit`, `push`, `advance_origin`, ...) stand in for what the user and other people do to the repo.
//!
//! Clones share state, so a test can keep a handle while the code under test owns another.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use shepherd_core::ports::worktrees::{managed_parts, normalize_lexically};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, Created, RepoRef, Worktree, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
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
    repos: Vec<Repo>,
    fail_next: Option<WorktreeError>,
}

#[derive(Debug, Clone)]
pub struct FakeWorktrees {
    root: PathBuf,
    state: Arc<Mutex<State>>,
}

impl FakeWorktrees {
    /// A fake managing worktrees under `root`, with no repos.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: Arc::default(),
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

    /// The next port call fails with `error` and changes nothing.
    pub fn fail_next(&self, error: WorktreeError) {
        self.lock().fail_next = Some(error);
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

    /// An opaque name for the commit checked out at `path`.
    pub fn head(&self, path: &Path) -> String {
        let s = self.lock();
        let (r, head) = s
            .checkout(path)
            .unwrap_or_else(|| panic!("no checkout at {path:?}"));
        format!("c{}", s.repos[r].resolve(&head).unwrap_or(usize::MAX))
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
        format!("c{}", refs.get(branch).copied().unwrap_or(usize::MAX))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take_failure(&self) -> Result<MutexGuard<'_, State>, WorktreeError> {
        let mut s = self.lock();
        match s.fail_next.take() {
            Some(e) => Err(e),
            None => Ok(s),
        }
    }
}

impl State {
    fn new_commit(&mut self, parents: Vec<Commit>) -> Commit {
        self.parents.push(parents);
        self.parents.len() - 1
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
        let mut s = self.take_failure()?;
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
        })
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::contracts::worktrees::{DEFAULT, REPO_NAME, WorktreesFixture};

    struct FakeFixture {
        fake: FakeWorktrees,
        repo: RepoRef,
    }

    impl FakeFixture {
        fn new() -> Self {
            let fake = FakeWorktrees::new("/fake/root");
            let repo = fake.add_repo(Path::new("/fake").join(REPO_NAME), DEFAULT);
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

        fn forget_origin_head(&self) {
            self.fake.forget_origin_head(&self.repo);
        }

        fn same_named_repo(&self) -> RepoRef {
            self.fake
                .add_repo(Path::new("/fake/other").join(REPO_NAME), DEFAULT)
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
