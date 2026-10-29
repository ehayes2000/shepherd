//! The `Worktrees` contract.
//!
//! Each case gets a fresh [`WorktreesFixture`]: an "origin" whose default branch is `trunk` (deliberately not `main`),
//! and a clone of it in a directory named `app`, next to the managed root. The fixture can also play the world around
//! the port: commit in a worktree, push, land a branch on origin, cut origin off.
//!
//! Cases that need a real filesystem (symlinks) are in [`FsWorktreesFixture`]; the fake has no filesystem, so it runs
//! only the first set.
//!
//! Run it with [`worktrees_contract!`](crate::worktrees_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `Worktrees` contract case. `$fixture` is an expression building a fresh
/// [`WorktreesFixture`] for each test:
///
/// - `worktrees_contract!(FakeFixture::new())` runs the cases every implementation must pass.
/// - `worktrees_contract!(fs: GitFixture::new())` also runs the filesystem cases; the fixture must implement
///   [`FsWorktreesFixture`].
///
/// New cases go in the lists below.
#[macro_export]
macro_rules! worktrees_contract {
    (fs: $fixture:expr) => {
        $crate::worktrees_contract!($fixture);
        $crate::worktrees_contract!(@cases $fixture;
            create_makes_a_checkout_on_disk,
            remove_deletes_the_directory,
            remove_refuses_a_symlink_to_a_worktree_outside_the_root,
            remove_refuses_a_symlinked_repo_dir_pointing_outside,
        );
    };
    ($fixture:expr) => {
        $crate::worktrees_contract!(@cases $fixture;
            default_branch_is_what_origin_head_points_at,
            default_branch_of_a_non_repo_is_not_a_repo,
            default_branch_repairs_a_missing_origin_head,
            default_branch_without_origin_head_offline_is_no_default_branch,
            create_places_the_worktree_under_root_repo_name_and_name,
            create_branches_from_freshly_fetched_origin_default,
            create_branch_has_no_upstream,
            create_from_inside_a_worktree_uses_the_same_repo_name,
            create_with_a_taken_name_is_branch_exists,
            create_after_remove_is_still_branch_exists,
            create_in_a_non_repo_is_not_a_repo,
            create_in_a_same_named_repo_is_repo_name_taken,
            create_offline_falls_back_to_last_fetched_origin_default,
            create_offline_without_remote_tracking_ref_falls_back_to_local_default,
            create_offline_with_nothing_to_fall_back_to_is_fetch_failed,
            create_when_fetch_hangs_times_out_and_falls_back,
            concurrent_creates_all_succeed,
            list_managed_lists_created_worktrees_only,
            list_managed_of_a_non_repo_is_not_a_repo,
            status_of_a_fresh_worktree_is_clean_merged_and_unpushed,
            status_counts_commits_ahead_of_origin_default,
            status_is_dirty_with_an_untracked_file,
            status_is_pushed_until_a_new_commit,
            status_is_merged_once_the_branch_lands_on_origin_default,
            status_of_a_missing_path_is_not_a_repo,
            remove_forgets_the_worktree,
            remove_dirty_without_force_is_dirty_and_keeps_it,
            remove_dirty_with_force_removes_it,
            remove_refuses_paths_outside_the_root,
            remove_refuses_dotdot_escapes,
            remove_refuses_the_root_and_repo_dirs,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::worktrees::$case(&$fixture);
            }
        )*
    };
}

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, RepoRef, WorktreeError, WorktreeName, WorktreePath,
    WorktreeStatus, Worktrees,
};

/// The origin's default branch in every fixture.
pub const DEFAULT: &str = "trunk";
/// The directory name of the fixture's clone, which is also its repo name under the root.
pub const REPO_NAME: &str = "app";

/// One repo (a clone of an origin) plus a port managing worktrees for it.
pub trait WorktreesFixture {
    type Port: Worktrees + Sync;

    fn port(&self) -> &Self::Port;
    /// The managed root the port was built with.
    fn root(&self) -> PathBuf;
    /// The clone's main checkout: a directory named [`REPO_NAME`], a sibling of [`WorktreesFixture::root`].
    fn repo(&self) -> RepoRef;
    /// A directory that is not inside any repo.
    fn not_a_repo(&self) -> RepoRef;

    /// The commit checked out at `path`, in whatever form the implementation names commits.
    fn head(&self, path: &Path) -> String;
    /// The commit at the tip of the default branch on origin itself (not the clone's view of it).
    fn origin_tip(&self) -> String;
    /// The commit at the tip of the clone's local default branch.
    fn local_default_tip(&self) -> String;

    /// Someone else pushes a new commit to origin's default branch. The clone has not fetched it.
    fn advance_origin(&self);
    /// Commits a new file on the branch checked out at `path`.
    fn commit(&self, path: &WorktreePath);
    /// Leaves an untracked file at `path`.
    fn make_dirty(&self, path: &WorktreePath);
    /// Pushes the branch at `path` to a branch of the same name on origin, setting it as upstream.
    fn push(&self, path: &WorktreePath);
    /// Pushes the HEAD of `path` to origin's default branch (a fast-forward), then fetches it into the clone.
    fn land_on_origin_default(&self, path: &WorktreePath);
    /// Makes origin unreachable: every fetch from now on fails quickly.
    fn go_offline(&self);
    /// Makes every fetch from now on hang for at least 10 minutes, and returns a port for the same repo and root whose
    /// fetch timeout is `timeout`.
    fn hang_origin(&self, timeout: Duration) -> Self::Port;
    /// Deletes the clone's local default branch (the main checkout is detached first).
    fn delete_local_default(&self);
    /// The clone fetches origin. Local branches don't move.
    fn fetch(&self);
    /// Deletes the clone's `origin/<default>` ref, leaving `origin/HEAD` pointing at it.
    fn delete_remote_tracking_default(&self);
    /// Deletes the clone's `origin/HEAD`, as if it had been made with `git init` + `git remote add` + `git fetch`.
    fn forget_origin_head(&self);
    /// Another clone whose directory is also named [`REPO_NAME`], somewhere else.
    fn same_named_repo(&self) -> RepoRef;
}

/// A fixture backed by a real filesystem.
pub trait FsWorktreesFixture: WorktreesFixture {
    /// A worktree of the repo outside the managed root.
    fn unmanaged_worktree(&self) -> WorktreePath;
    /// Creates a symlink at `link` (inside the root) pointing at `target`, creating `link`'s parents.
    fn symlink(&self, link: &Path, target: &Path);
}

fn name(s: &str) -> WorktreeName {
    WorktreeName::new(s).unwrap()
}

fn default() -> BranchName {
    BranchName::new(DEFAULT)
}

fn create<F: WorktreesFixture>(f: &F, n: &str) -> WorktreePath {
    f.port()
        .create(&f.repo(), &name(n), BaseSpec::OriginDefault)
        .unwrap()
        .path
}

fn listed_paths<F: WorktreesFixture>(f: &F) -> Vec<WorktreePath> {
    let mut paths: Vec<_> = f
        .port()
        .list_managed(&f.repo())
        .unwrap()
        .into_iter()
        .map(|w| w.path)
        .collect();
    paths.sort();
    paths
}

fn assert_outside<F: WorktreesFixture>(f: &F, path: &Path) {
    let result = f.port().remove(&WorktreePath::new(path), true);
    assert!(
        matches!(result, Err(WorktreeError::OutsideManagedRoot(_))),
        "remove({}) gave {result:?}",
        path.display()
    );
}

// ---------- default_branch ----------

pub fn default_branch_is_what_origin_head_points_at<F: WorktreesFixture>(f: &F) {
    assert_eq!(f.port().default_branch(&f.repo()), Ok(default()));
}

pub fn default_branch_of_a_non_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f.port().default_branch(&f.not_a_repo());
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

pub fn default_branch_repairs_a_missing_origin_head<F: WorktreesFixture>(f: &F) {
    f.forget_origin_head();
    assert_eq!(f.port().default_branch(&f.repo()), Ok(default()));
    let created = f
        .port()
        .create(&f.repo(), &name("repaired"), BaseSpec::OriginDefault)
        .unwrap();
    assert_eq!(created.base, BaseUsed::Origin { branch: default() });
}

pub fn default_branch_without_origin_head_offline_is_no_default_branch<F: WorktreesFixture>(f: &F) {
    f.forget_origin_head();
    f.go_offline();
    let result = f.port().default_branch(&f.repo());
    assert!(
        matches!(result, Err(WorktreeError::NoDefaultBranch(_))),
        "{result:?}"
    );
}

// ---------- create ----------

pub fn create_places_the_worktree_under_root_repo_name_and_name<F: WorktreesFixture>(f: &F) {
    let created = f
        .port()
        .create(&f.repo(), &name("fix-login"), BaseSpec::OriginDefault)
        .unwrap();
    assert_eq!(
        created.path,
        WorktreePath::new(f.root().join(REPO_NAME).join("fix-login"))
    );
    assert_eq!(created.branch, BranchName::new("fix-login"));
}

pub fn create_branches_from_freshly_fetched_origin_default<F: WorktreesFixture>(f: &F) {
    f.advance_origin();
    let created = f
        .port()
        .create(&f.repo(), &name("fresh"), BaseSpec::OriginDefault)
        .unwrap();
    assert_eq!(created.base, BaseUsed::Origin { branch: default() });
    assert_eq!(f.head(created.path.as_path()), f.origin_tip());
    assert_ne!(
        f.origin_tip(),
        f.local_default_tip(),
        "the local default should not have moved"
    );
}

/// An upstream would make a brand-new branch look pushed.
pub fn create_branch_has_no_upstream<F: WorktreesFixture>(f: &F) {
    let path = create(f, "fresh");
    assert!(!f.port().status(&path).unwrap().pushed);
}

pub fn create_from_inside_a_worktree_uses_the_same_repo_name<F: WorktreesFixture>(f: &F) {
    let first = create(f, "first");
    let second = f
        .port()
        .create(
            &RepoRef::new(first.as_path()),
            &name("second"),
            BaseSpec::OriginDefault,
        )
        .unwrap();
    assert_eq!(
        second.path,
        WorktreePath::new(f.root().join(REPO_NAME).join("second"))
    );
    assert_eq!(listed_paths(f).len(), 2);
}

pub fn create_with_a_taken_name_is_branch_exists<F: WorktreesFixture>(f: &F) {
    create(f, "twice");
    let result = f
        .port()
        .create(&f.repo(), &name("twice"), BaseSpec::OriginDefault);
    assert_eq!(
        result,
        Err(WorktreeError::BranchExists(BranchName::new("twice")))
    );
}

/// Removing a worktree keeps its branch (it may hold unpushed work), so the name stays taken.
pub fn create_after_remove_is_still_branch_exists<F: WorktreesFixture>(f: &F) {
    let path = create(f, "again");
    f.port().remove(&path, false).unwrap();
    let result = f
        .port()
        .create(&f.repo(), &name("again"), BaseSpec::OriginDefault);
    assert_eq!(
        result,
        Err(WorktreeError::BranchExists(BranchName::new("again")))
    );
}

/// Two repos in directories with the same name map to the same `<root>/<name>/`; the second can't use it while the
/// first has worktrees there.
pub fn create_in_a_same_named_repo_is_repo_name_taken<F: WorktreesFixture>(f: &F) {
    let first = create(f, "first");
    let other = f.same_named_repo();
    let result = f
        .port()
        .create(&other, &name("second"), BaseSpec::OriginDefault);
    assert_eq!(
        result,
        Err(WorktreeError::RepoNameTaken {
            path: f.root().join(REPO_NAME)
        })
    );
    assert_eq!(listed_paths(f), vec![first.clone()]);

    f.port().remove(&first, false).unwrap();
    let created = f
        .port()
        .create(&other, &name("second"), BaseSpec::OriginDefault)
        .unwrap();
    assert_eq!(
        created.path,
        WorktreePath::new(f.root().join(REPO_NAME).join("second"))
    );
    assert!(listed_paths(f).is_empty(), "not the first repo's worktree");
}

pub fn create_in_a_non_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f
        .port()
        .create(&f.not_a_repo(), &name("x"), BaseSpec::OriginDefault);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

/// Asserts `base` is the given fallback kind for the default branch, with the fetch error kept.
fn assert_fallback(base: &BaseUsed, stale: bool) {
    let (BaseUsed::Stale {
        branch,
        fetch_error,
    }
    | BaseUsed::Local {
        branch,
        fetch_error,
    }) = base
    else {
        panic!("expected a fallback, got {base:?}");
    };
    assert_eq!(matches!(base, BaseUsed::Stale { .. }), stale, "{base:?}");
    assert_eq!(branch, &default());
    assert!(!fetch_error.is_empty(), "the fetch error should be kept");
}

/// The last-fetched `origin/<default>` is usually newer than the local default.
pub fn create_offline_falls_back_to_last_fetched_origin_default<F: WorktreesFixture>(f: &F) {
    f.advance_origin();
    f.fetch();
    let last_fetched = f.origin_tip();
    f.advance_origin();
    f.go_offline();
    let created = f
        .port()
        .create(&f.repo(), &name("offline"), BaseSpec::OriginDefault)
        .unwrap();
    assert_fallback(&created.base, true);
    let head = f.head(created.path.as_path());
    assert_eq!(head, last_fetched);
    assert_ne!(head, f.local_default_tip());
    assert_ne!(head, f.origin_tip());
}

pub fn create_offline_without_remote_tracking_ref_falls_back_to_local_default<
    F: WorktreesFixture,
>(
    f: &F,
) {
    f.delete_remote_tracking_default();
    f.go_offline();
    let created = f
        .port()
        .create(&f.repo(), &name("offline"), BaseSpec::OriginDefault)
        .unwrap();
    assert_fallback(&created.base, false);
    assert_eq!(f.head(created.path.as_path()), f.local_default_tip());
}

pub fn create_offline_with_nothing_to_fall_back_to_is_fetch_failed<F: WorktreesFixture>(f: &F) {
    f.delete_remote_tracking_default();
    f.delete_local_default();
    f.go_offline();
    let result = f
        .port()
        .create(&f.repo(), &name("stranded"), BaseSpec::OriginDefault);
    assert!(
        matches!(result, Err(WorktreeError::FetchFailed(_))),
        "{result:?}"
    );
    assert!(listed_paths(f).is_empty());
}

/// The time bound is loose because the case must pass under heavy load; a fetch without a timeout would hang for at
/// least 10 minutes.
pub fn create_when_fetch_hangs_times_out_and_falls_back<F: WorktreesFixture>(f: &F) {
    let port = f.hang_origin(Duration::from_millis(300));
    let started = Instant::now();
    let created = port
        .create(&f.repo(), &name("hung"), BaseSpec::OriginDefault)
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(300),
        "took {:?}",
        started.elapsed()
    );
    assert_fallback(&created.base, true);
}

pub fn concurrent_creates_all_succeed<F: WorktreesFixture>(f: &F) {
    let repo = f.repo();
    let port = f.port();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let repo = &repo;
                s.spawn(move || {
                    port.create(repo, &name(&format!("par-{i}")), BaseSpec::OriginDefault)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in &results {
        let created = result.as_ref().unwrap();
        assert_eq!(created.base, BaseUsed::Origin { branch: default() });
    }
    assert_eq!(listed_paths(f).len(), 4);
}

// ---------- list_managed ----------

pub fn list_managed_lists_created_worktrees_only<F: WorktreesFixture>(f: &F) {
    assert!(
        listed_paths(f).is_empty(),
        "the main checkout is not managed"
    );
    let a = create(f, "a");
    let b = create(f, "b");
    let mut listed = f.port().list_managed(&f.repo()).unwrap();
    listed.sort_by(|x, y| x.path.cmp(&y.path));
    let expected = [(a, "a"), (b, "b")];
    assert_eq!(listed.len(), 2, "{listed:?}");
    for (worktree, (path, branch)) in listed.iter().zip(expected) {
        assert_eq!(worktree.path, path);
        assert_eq!(worktree.branch, Some(BranchName::new(branch)));
    }
}

pub fn list_managed_of_a_non_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f.port().list_managed(&f.not_a_repo());
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

// ---------- status ----------

pub fn status_of_a_fresh_worktree_is_clean_merged_and_unpushed<F: WorktreesFixture>(f: &F) {
    let path = create(f, "fresh");
    assert_eq!(
        f.port().status(&path),
        Ok(WorktreeStatus {
            dirty: false,
            ahead: 0,
            pushed: false,
            merged_into_default: true,
        })
    );
}

pub fn status_counts_commits_ahead_of_origin_default<F: WorktreesFixture>(f: &F) {
    let path = create(f, "work");
    f.commit(&path);
    f.commit(&path);
    let status = f.port().status(&path).unwrap();
    assert_eq!(status.ahead, 2);
    assert!(!status.merged_into_default);
    assert!(!status.dirty);
}

pub fn status_is_dirty_with_an_untracked_file<F: WorktreesFixture>(f: &F) {
    let path = create(f, "messy");
    f.make_dirty(&path);
    assert!(f.port().status(&path).unwrap().dirty);
}

pub fn status_is_pushed_until_a_new_commit<F: WorktreesFixture>(f: &F) {
    let path = create(f, "shared");
    f.commit(&path);
    f.push(&path);
    let status = f.port().status(&path).unwrap();
    assert!(status.pushed, "{status:?}");
    assert_eq!(status.ahead, 1, "pushed is not merged");
    assert!(!status.merged_into_default);

    f.commit(&path);
    assert!(!f.port().status(&path).unwrap().pushed);
}

pub fn status_is_merged_once_the_branch_lands_on_origin_default<F: WorktreesFixture>(f: &F) {
    let path = create(f, "feature");
    f.commit(&path);
    assert!(!f.port().status(&path).unwrap().merged_into_default);
    f.land_on_origin_default(&path);
    let status = f.port().status(&path).unwrap();
    assert!(status.merged_into_default, "{status:?}");
    assert_eq!(status.ahead, 0);
}

pub fn status_of_a_missing_path_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let path = WorktreePath::new(f.root().join(REPO_NAME).join("never-created"));
    let result = f.port().status(&path);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

// ---------- remove ----------

pub fn remove_forgets_the_worktree<F: WorktreesFixture>(f: &F) {
    let keep = create(f, "keep");
    let gone = create(f, "gone");
    f.port().remove(&gone, false).unwrap();
    assert_eq!(listed_paths(f), vec![keep]);
    assert!(matches!(
        f.port().status(&gone),
        Err(WorktreeError::NotARepo(_))
    ));
}

pub fn remove_dirty_without_force_is_dirty_and_keeps_it<F: WorktreesFixture>(f: &F) {
    let path = create(f, "messy");
    f.make_dirty(&path);
    assert_eq!(
        f.port().remove(&path, false),
        Err(WorktreeError::Dirty(path.clone()))
    );
    assert_eq!(listed_paths(f), vec![path.clone()]);
    assert!(f.port().status(&path).unwrap().dirty);
}

pub fn remove_dirty_with_force_removes_it<F: WorktreesFixture>(f: &F) {
    let path = create(f, "messy");
    f.make_dirty(&path);
    f.port().remove(&path, true).unwrap();
    assert!(listed_paths(f).is_empty());
}

pub fn remove_refuses_paths_outside_the_root<F: WorktreesFixture>(f: &F) {
    assert_outside(f, f.repo().as_path());
    assert_outside(f, Path::new("/"));
    assert_eq!(f.port().default_branch(&f.repo()), Ok(default()));
}

/// `<root>/app/x/../../../app` is the main checkout.
pub fn remove_refuses_dotdot_escapes<F: WorktreesFixture>(f: &F) {
    create(f, "x");
    let escape = f
        .root()
        .join(REPO_NAME)
        .join("x")
        .join("..")
        .join("..")
        .join("..")
        .join(REPO_NAME);
    assert_outside(f, &escape);
    // Lands on a managed-looking depth but still outside.
    let sneaky = f.root().join("..").join("..").join("x").join("y");
    assert_outside(f, &sneaky);
    assert_eq!(f.port().default_branch(&f.repo()), Ok(default()));
    assert_eq!(listed_paths(f).len(), 1);
}

pub fn remove_refuses_the_root_and_repo_dirs<F: WorktreesFixture>(f: &F) {
    let path = create(f, "x");
    assert_outside(f, &f.root());
    assert_outside(f, &f.root().join(REPO_NAME));
    assert_outside(f, &path.as_path().join("src"));
    assert_eq!(listed_paths(f), vec![path]);
}

// ---------- filesystem ----------

pub fn create_makes_a_checkout_on_disk<F: FsWorktreesFixture>(f: &F) {
    let path = create(f, "disk");
    assert!(path.as_path().join(".git").exists(), "{path}");
}

pub fn remove_deletes_the_directory<F: FsWorktreesFixture>(f: &F) {
    let path = create(f, "disk");
    f.port().remove(&path, false).unwrap();
    assert!(!path.as_path().exists(), "{path}");
}

pub fn remove_refuses_a_symlink_to_a_worktree_outside_the_root<F: FsWorktreesFixture>(f: &F) {
    let outside = f.unmanaged_worktree();
    let link = f.root().join(REPO_NAME).join("link");
    f.symlink(&link, outside.as_path());
    assert_outside(f, &link);
    assert!(outside.as_path().join(".git").exists(), "{outside}");
    assert!(f.port().status(&outside).is_ok());
}

/// `<root>/evil -> <outside dir>`, so `<root>/evil/<name>` looks managed but is not.
pub fn remove_refuses_a_symlinked_repo_dir_pointing_outside<F: FsWorktreesFixture>(f: &F) {
    let outside = f.unmanaged_worktree();
    let parent = outside.as_path().parent().unwrap();
    let name = outside.as_path().file_name().unwrap();
    let link = f.root().join("evil");
    f.symlink(&link, parent);
    assert_outside(f, &link.join(name));
    assert!(outside.as_path().join(".git").exists(), "{outside}");
}
