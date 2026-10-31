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
            repo_root_through_a_symlink_is_the_resolved_checkout,
            create_makes_a_checkout_on_disk,
            remove_deletes_the_directory,
            remove_refuses_a_symlink_to_a_worktree_outside_the_root,
            remove_refuses_a_symlinked_repo_dir_pointing_outside,
        );
    };
    ($fixture:expr) => {
        $crate::worktrees_contract!(@cases $fixture;
            repo_root_of_the_main_checkout_is_itself,
            repo_root_of_a_subdirectory_is_the_checkout,
            repo_root_of_a_nested_subdirectory_and_its_parent_is_the_checkout,
            repo_root_of_a_linked_worktree_is_the_main_checkout,
            repo_root_of_a_non_repo_is_not_a_repo,
            repo_root_of_a_missing_path_inside_the_repo_is_not_a_repo,
            default_branch_is_what_origin_head_points_at,
            default_branch_of_a_non_repo_is_not_a_repo,
            default_branch_repairs_a_missing_origin_head,
            default_branch_without_origin_head_offline_is_no_default_branch,
            create_places_the_worktree_under_root_repo_name_and_name,
            create_branches_from_freshly_fetched_origin_default,
            create_branch_has_no_upstream,
            create_from_inside_a_worktree_uses_the_same_repo_name,
            a_subdirectory_of_the_repo_names_the_repo,
            create_with_a_taken_name_is_branch_exists,
            create_after_remove_is_still_branch_exists,
            create_in_a_non_repo_is_not_a_repo,
            create_in_a_same_named_repo_is_repo_name_taken,
            create_offline_falls_back_to_last_fetched_origin_default,
            create_offline_without_remote_tracking_ref_falls_back_to_local_default,
            create_offline_with_nothing_to_fall_back_to_is_fetch_failed,
            create_when_fetch_hangs_times_out_and_falls_back,
            concurrent_creates_all_succeed,
            a_create_cancelled_while_it_runs_makes_nothing_and_can_be_retried,
            a_create_cancelled_before_it_starts_makes_nothing_and_can_be_retried,
            a_cancel_leaves_creates_of_other_names_alone,
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
            remove_of_a_missing_managed_path_is_not_a_repo,
            remove_refuses_paths_outside_the_root,
            remove_refuses_dotdot_escapes,
            remove_refuses_the_root_and_repo_dirs,
            create_reports_the_commit_it_started_from,
            commits_since_the_base_of_a_fresh_worktree_are_none,
            commits_since_the_base_are_the_worktrees_commits_oldest_first,
            commits_since_a_later_base_leave_out_what_it_has,
            a_rebased_commit_keeps_its_patch_id,
            commits_since_a_base_the_repo_lacks_is_an_error,
            commits_since_in_a_missing_path_is_not_a_repo,
            root_commit_is_the_same_from_every_checkout_of_the_repo,
            merges_and_empty_commits_have_no_patch_id,
            root_commit_of_a_history_with_two_roots_is_the_smaller,
            root_commit_of_a_non_repo_is_not_a_repo,
            patch_id_of_a_commit_is_the_one_commits_since_gives_it,
            patch_id_of_a_merge_or_an_empty_commit_is_none,
            patch_id_of_a_commit_the_repo_lacks_is_an_error,
            patch_id_outside_a_repo_is_not_a_repo,
            resolve_commit_names_hashes_abbreviations_head_and_branches,
            resolve_commit_of_a_rev_that_names_no_commit_is_none,
            resolve_commit_never_reads_a_rev_as_an_option,
            resolve_commit_outside_a_repo_is_not_a_repo,
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
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, CommitHash, PatchId, RepoRef, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
};

/// The origin's default branch in every fixture.
pub const DEFAULT: &str = "trunk";
/// The directory name of the fixture's clone, which is also its repo name under the root.
pub const REPO_NAME: &str = "app";
/// A directory inside the clone.
pub const SUBDIR: &str = "src";
/// A directory inside [`SUBDIR`].
pub const NESTED: &str = "deep";

/// A liveness bound on one create, or on waiting for one to get somewhere, never a measurement. Generous: on a heavily
/// loaded machine each of a create's git calls can take minutes (F17). Still well short of the hour a fetch hangs for
/// in [`WorktreesFixture::hang_origin`].
pub const CREATE_LIMIT: Duration = Duration::from_secs(1800);

/// One repo (a clone of an origin) plus a port managing worktrees for it.
pub trait WorktreesFixture {
    type Port: Worktrees + Sync;

    fn port(&self) -> &Self::Port;
    /// The managed root the port was built with.
    fn root(&self) -> PathBuf;
    /// The clone's main checkout: a directory named [`REPO_NAME`], a sibling of [`WorktreesFixture::root`]. It has a
    /// tracked subdirectory [`SUBDIR`], which has a tracked subdirectory [`NESTED`].
    fn repo(&self) -> RepoRef;
    /// A directory that is not inside any repo.
    fn not_a_repo(&self) -> RepoRef;

    /// The commit checked out at `path`, as a [`CommitHash`] spells it.
    fn head(&self, path: &Path) -> String;
    /// The commit at the tip of the default branch on origin itself (not the clone's view of it), as a [`CommitHash`]
    /// spells it.
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
    /// Makes every fetch from now on hang for at least an hour, and returns a port for the same repo and root whose
    /// fetch timeout is `timeout`.
    fn hang_origin(&self, timeout: Duration) -> Self::Port;
    /// Deletes the clone's local default branch (the main checkout is detached first).
    fn delete_local_default(&self);
    /// The clone fetches origin. Local branches don't move.
    fn fetch(&self);
    /// Fetches, then rebases the branch checked out at `path` onto the clone's `origin/<default>`: its commits are
    /// made again there, with the same changes.
    fn rebase_onto_origin_default(&self, path: &WorktreePath);
    /// Merges a new root commit (an unrelated history) into the branch checked out at `path`, and returns that root,
    /// as a [`CommitHash`] spells it.
    fn merge_unrelated_root(&self, path: &WorktreePath) -> String;
    /// Makes a commit that changes nothing (`--allow-empty`) on the branch checked out at `path`.
    fn commit_empty(&self, path: &WorktreePath);
    /// Deletes the clone's `origin/<default>` ref, leaving `origin/HEAD` pointing at it.
    fn delete_remote_tracking_default(&self);
    /// Deletes the clone's `origin/HEAD`, as if it had been made with `git init` + `git remote add` + `git fetch`.
    fn forget_origin_head(&self);
    /// Another clone whose directory is also named [`REPO_NAME`], somewhere else.
    fn same_named_repo(&self) -> RepoRef;
    /// `path` with symlinks resolved, as the port reports paths it found on its own (e.g. a tempdir behind `/var`).
    fn resolved(&self, path: &Path) -> PathBuf;
    /// The next create blocks while it makes the worktree (for git: in the checkout, after the branch is made and the
    /// worktree registered) until the returned handle is dropped. Whether creates after it are held too, while the
    /// handle lives, is up to the fixture (the git fixture's hook holds every one), so cases hold one create at a time.
    fn hold_next_create(&self) -> Box<dyn HeldCreate + '_>;
}

/// A create a [`WorktreesFixture::hold_next_create`] holds. Dropping it lets the create go on.
pub trait HeldCreate {
    /// Blocks until the held create is waiting, up to `limit` (a liveness bound). Whether it is. It can be asked again
    /// after it said no.
    fn wait_entered(&self, limit: Duration) -> bool;
}

/// Waits for `held` to say its create is waiting, while that create runs on another thread that sends its result on
/// `answer`. `Err` says why it isn't: the create answered first (with what, so a failure shows the create's own error),
/// or `limit` passed with the create still running. `limit` is a liveness bound, never a measurement: on a heavily
/// loaded machine every git call before the hold can take minutes (F17).
pub fn wait_held<T: std::fmt::Debug>(
    held: &(impl HeldCreate + ?Sized),
    answer: &Receiver<T>,
    limit: Duration,
) -> Result<(), String> {
    /// How long each wait for the hold lasts before the answer is looked at again.
    const SLICE: Duration = Duration::from_millis(50);
    let started = Instant::now();
    loop {
        if held.wait_entered(SLICE) {
            return Ok(());
        }
        match answer.try_recv() {
            Ok(answer) => {
                return Err(format!(
                    "the create answered before it was held: {answer:?}"
                ));
            }
            Err(TryRecvError::Disconnected) => {
                return Err("the create's thread ended without answering".into());
            }
            Err(TryRecvError::Empty) => {}
        }
        if started.elapsed() >= limit {
            return Err(format!(
                "the create was not held within {limit:?}, and is still running"
            ));
        }
    }
}

impl HeldCreate for crate::fakes::hold::Hold {
    fn wait_entered(&self, limit: Duration) -> bool {
        crate::fakes::hold::Hold::wait_entered(self, limit)
    }
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

// ---------- repo_root ----------

fn main_checkout<F: WorktreesFixture>(f: &F) -> Result<PathBuf, WorktreeError> {
    Ok(f.resolved(f.repo().as_path()))
}

pub fn repo_root_of_the_main_checkout_is_itself<F: WorktreesFixture>(f: &F) {
    assert_eq!(f.port().repo_root(f.repo().as_path()), main_checkout(f));
}

pub fn repo_root_of_a_subdirectory_is_the_checkout<F: WorktreesFixture>(f: &F) {
    let sub = f.repo().as_path().join(SUBDIR);
    assert_eq!(f.port().repo_root(&sub), main_checkout(f));
}

/// A directory exists when a directory below it does.
pub fn repo_root_of_a_nested_subdirectory_and_its_parent_is_the_checkout<F: WorktreesFixture>(
    f: &F,
) {
    let sub = f.repo().as_path().join(SUBDIR);
    assert_eq!(f.port().repo_root(&sub.join(NESTED)), main_checkout(f));
    assert_eq!(f.port().repo_root(&sub), main_checkout(f));
}

/// A worktree is another checkout of the same repo, which is named by its main checkout.
pub fn repo_root_of_a_linked_worktree_is_the_main_checkout<F: WorktreesFixture>(f: &F) {
    let path = create(f, "linked");
    assert_eq!(f.port().repo_root(path.as_path()), main_checkout(f));
}

pub fn repo_root_of_a_non_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f.port().repo_root(f.not_a_repo().as_path());
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
    let missing = f.not_a_repo().as_path().join("missing");
    let result = f.port().repo_root(&missing);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

pub fn repo_root_of_a_missing_path_inside_the_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let missing = f.repo().as_path().join("missing");
    assert_eq!(
        f.port().repo_root(&missing),
        Err(WorktreeError::NotARepo(missing.clone()))
    );
    let deeper = f.repo().as_path().join(SUBDIR).join("missing");
    assert_eq!(
        f.port().repo_root(&deeper),
        Err(WorktreeError::NotARepo(deeper.clone()))
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

/// Like git, any path inside a checkout names its repo.
pub fn a_subdirectory_of_the_repo_names_the_repo<F: WorktreesFixture>(f: &F) {
    let sub = RepoRef::new(f.repo().as_path().join(SUBDIR));
    assert_eq!(f.port().default_branch(&sub), Ok(default()));
    let created = f
        .port()
        .create(&sub, &name("from-sub"), BaseSpec::OriginDefault)
        .unwrap();
    assert_eq!(
        created.path,
        WorktreePath::new(f.root().join(REPO_NAME).join("from-sub"))
    );
    let listed: Vec<_> = f
        .port()
        .list_managed(&sub)
        .unwrap()
        .into_iter()
        .map(|w| w.path)
        .collect();
    assert_eq!(listed, vec![created.path]);
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

/// A fetch without a timeout would hang for at least an hour, so a create that returns within [`CREATE_LIMIT`] (a
/// liveness bound, never a measurement) timed it out.
pub fn create_when_fetch_hangs_times_out_and_falls_back<F: WorktreesFixture>(f: &F) {
    let port = f.hang_origin(Duration::from_millis(300));
    let started = Instant::now();
    let created = port
        .create(&f.repo(), &name("hung"), BaseSpec::OriginDefault)
        .unwrap();
    assert!(
        started.elapsed() < CREATE_LIMIT,
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

// ---------- cancel_create ----------

/// A create cancelled while it makes the worktree stops, and leaves nothing: a retry with the same name succeeds.
pub fn a_create_cancelled_while_it_runs_makes_nothing_and_can_be_retried<F: WorktreesFixture>(
    f: &F,
) {
    let repo = f.repo();
    let port = f.port();
    let mut held = Some(f.hold_next_create());
    let repo = &repo;
    // A cancel that doesn't stop the create fails the case (after releasing the hold), never hangs it.
    let result = std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel();
        s.spawn(move || drop(tx.send(port.create(repo, &name("doomed"), BaseSpec::OriginDefault))));
        if let Err(why) = wait_held(held.as_deref().unwrap(), &rx, CREATE_LIMIT) {
            // Cancelled before the hold is released, so a create that reaches the hold only after that can't wait
            // there for good, and the scope with it.
            port.cancel_create(repo, &name("doomed"));
            drop(held.take());
            panic!("{why}");
        }
        port.cancel_create(repo, &name("doomed"));
        let answer = rx.recv_timeout(CREATE_LIMIT);
        drop(held.take());
        answer.unwrap_or_else(|_| panic!("the cancelled create never answered"))
    });
    assert_eq!(result, Err(WorktreeError::Cancelled));
    assert!(listed_paths(f).is_empty());
    create(f, "doomed");
}

/// A cancel that comes before its create starts (the create's thread is slow to start) still cancels it.
pub fn a_create_cancelled_before_it_starts_makes_nothing_and_can_be_retried<F: WorktreesFixture>(
    f: &F,
) {
    f.port().cancel_create(&f.repo(), &name("early"));
    let result = f
        .port()
        .create(&f.repo(), &name("early"), BaseSpec::OriginDefault);
    assert_eq!(result, Err(WorktreeError::Cancelled));
    assert!(listed_paths(f).is_empty());
    create(f, "early");
}

pub fn a_cancel_leaves_creates_of_other_names_alone<F: WorktreesFixture>(f: &F) {
    f.port().cancel_create(&f.repo(), &name("other"));
    let path = create(f, "mine");
    assert_eq!(listed_paths(f), vec![path]);
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

pub fn remove_of_a_missing_managed_path_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let path = create(f, "once");
    f.port().remove(&path, false).unwrap();
    let again = f.port().remove(&path, false);
    assert!(
        matches!(again, Err(WorktreeError::NotARepo(_))),
        "{again:?}"
    );
    let never = WorktreePath::new(f.root().join(REPO_NAME).join("never-created"));
    let result = f.port().remove(&never, true);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
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

// ---------- commits_since, root_commit ----------

fn hash(name: &str) -> CommitHash {
    CommitHash::parse(name).unwrap()
}

/// The base of the commits the worktree makes.
fn base_of<F: WorktreesFixture>(f: &F, n: &str) -> (WorktreePath, CommitHash) {
    let created = f
        .port()
        .create(&f.repo(), &name(n), BaseSpec::OriginDefault)
        .unwrap();
    let base = created
        .base_commit
        .unwrap_or_else(|| panic!("{n} has no base commit"));
    (created.path, base)
}

pub fn create_reports_the_commit_it_started_from<F: WorktreesFixture>(f: &F) {
    f.advance_origin();
    let (path, base) = base_of(f, "fresh");
    assert_eq!(base, hash(&f.head(path.as_path())));
    assert_eq!(base, hash(&f.origin_tip()));
}

pub fn commits_since_the_base_of_a_fresh_worktree_are_none<F: WorktreesFixture>(f: &F) {
    let (path, base) = base_of(f, "fresh");
    assert_eq!(f.port().commits_since(&path, &base), Ok(Vec::new()));
}

pub fn commits_since_the_base_are_the_worktrees_commits_oldest_first<F: WorktreesFixture>(f: &F) {
    let (path, base) = base_of(f, "work");
    f.commit(&path);
    let first = hash(&f.head(path.as_path()));
    f.commit(&path);
    let second = hash(&f.head(path.as_path()));
    let commits = f.port().commits_since(&path, &base).unwrap();
    let hashes: Vec<&CommitHash> = commits.iter().map(|(c, _)| c).collect();
    assert_eq!(hashes, [&first, &second]);
    let patches: Vec<&PatchId> = commits.iter().filter_map(|(_, p)| p.as_ref()).collect();
    assert_eq!(patches.len(), 2, "each commit changes a file: {commits:?}");
    assert_ne!(patches[0], patches[1], "different changes: {commits:?}");
}

/// Commits the base has (here, the first of the worktree's) are left out.
pub fn commits_since_a_later_base_leave_out_what_it_has<F: WorktreesFixture>(f: &F) {
    let (path, _) = base_of(f, "work");
    f.commit(&path);
    let first = hash(&f.head(path.as_path()));
    f.commit(&path);
    let second = hash(&f.head(path.as_path()));
    let commits = f.port().commits_since(&path, &first).unwrap();
    let hashes: Vec<&CommitHash> = commits.iter().map(|(c, _)| c).collect();
    assert_eq!(hashes, [&second]);
}

/// What finding a session's commit after its branch was rebased relies on.
pub fn a_rebased_commit_keeps_its_patch_id<F: WorktreesFixture>(f: &F) {
    let (path, base) = base_of(f, "work");
    f.commit(&path);
    f.commit(&path);
    let before = f.port().commits_since(&path, &base).unwrap();
    f.advance_origin();
    f.rebase_onto_origin_default(&path);
    let new_base = hash(&f.origin_tip());
    let after = f.port().commits_since(&path, &new_base).unwrap();
    assert_eq!(after.len(), 2, "{after:?}");
    for ((old, old_patch), (new, new_patch)) in before.iter().zip(&after) {
        assert_ne!(old, new, "a rebase makes new commits");
        assert!(old_patch.is_some(), "{before:?}");
        assert_eq!(old_patch, new_patch, "the same change");
    }
}

pub fn commits_since_a_base_the_repo_lacks_is_an_error<F: WorktreesFixture>(f: &F) {
    let (path, _) = base_of(f, "work");
    f.commit(&path);
    let unknown = hash(&"deadbeef".repeat(5));
    let result = f.port().commits_since(&path, &unknown);
    assert!(result.is_err(), "{result:?}");
}

pub fn commits_since_in_a_missing_path_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let (_, base) = base_of(f, "work");
    let path = WorktreePath::new(f.root().join(REPO_NAME).join("never-created"));
    let result = f.port().commits_since(&path, &base);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

pub fn root_commit_is_the_same_from_every_checkout_of_the_repo<F: WorktreesFixture>(f: &F) {
    let root = f.port().root_commit(&f.repo()).unwrap();
    let (path, _) = base_of(f, "work");
    f.commit(&path);
    f.advance_origin();
    f.fetch();
    assert_eq!(
        f.port().root_commit(&RepoRef::new(path.as_path())),
        Ok(root.clone())
    );
    assert_eq!(
        f.port()
            .root_commit(&RepoRef::new(f.repo().as_path().join(SUBDIR))),
        Ok(root)
    );
}

/// A merge and a commit that changes nothing print no patch, so they have no patch-id; the commits around them do.
pub fn merges_and_empty_commits_have_no_patch_id<F: WorktreesFixture>(f: &F) {
    let (path, base) = base_of(f, "work");
    f.commit(&path);
    f.commit_empty(&path);
    let empty = hash(&f.head(path.as_path()));
    let root = hash(&f.merge_unrelated_root(&path));
    let merge = hash(&f.head(path.as_path()));
    let commits = f.port().commits_since(&path, &base).unwrap();
    let without: Vec<&CommitHash> = commits
        .iter()
        .filter(|(_, p)| p.is_none())
        .map(|(c, _)| c)
        .collect();
    let mut expected = vec![&empty, &merge];
    expected.sort();
    let mut without = without;
    without.sort();
    assert_eq!(without, expected, "{commits:?}");
    assert!(
        commits.iter().any(|(c, p)| *c == root && p.is_some()),
        "the merged-in root has a patch: {commits:?}"
    );
    assert_eq!(commits.len(), 4, "{commits:?}");
}

/// A history with several roots (an unrelated one merged in) is named by the smallest, whichever it is.
pub fn root_commit_of_a_history_with_two_roots_is_the_smaller<F: WorktreesFixture>(f: &F) {
    let first = f.port().root_commit(&f.repo()).unwrap();
    let (path, _) = base_of(f, "work");
    let other = hash(&f.merge_unrelated_root(&path));
    assert_ne!(first.commit(), &other);
    let smaller = first.commit().clone().min(other);
    let root = f.port().root_commit(&RepoRef::new(path.as_path())).unwrap();
    assert_eq!(root.commit(), &smaller);
}

pub fn root_commit_of_a_non_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f.port().root_commit(&f.not_a_repo());
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

// ---------- patch_id, resolve_commit ----------

/// The same pinned patch as `commits_since`, so a commit found by its patch-id matches the one capture recorded; and a
/// rebased commit has the patch-id of the commit it was made from.
pub fn patch_id_of_a_commit_is_the_one_commits_since_gives_it<F: WorktreesFixture>(f: &F) {
    let (path, base) = base_of(f, "work");
    f.commit(&path);
    f.commit(&path);
    let before = f.port().commits_since(&path, &base).unwrap();
    let repo = RepoRef::new(path.as_path());
    for (commit, patch) in &before {
        assert!(patch.is_some(), "{before:?}");
        assert_eq!(f.port().patch_id(&repo, commit).as_ref(), Ok(patch));
    }
    f.advance_origin();
    f.rebase_onto_origin_default(&path);
    let after = f
        .port()
        .commits_since(&path, &hash(&f.origin_tip()))
        .unwrap();
    assert_eq!(after.len(), 2, "{after:?}");
    for ((old, old_patch), (new, _)) in before.iter().zip(&after) {
        assert_ne!(old, new);
        assert_eq!(f.port().patch_id(&repo, new).as_ref(), Ok(old_patch));
    }
    // From the main checkout too: every checkout of the repo has the worktree's commits.
    let (first, first_patch) = &before[0];
    assert_eq!(
        f.port().patch_id(&f.repo(), first).as_ref(),
        Ok(first_patch)
    );
}

pub fn patch_id_of_a_merge_or_an_empty_commit_is_none<F: WorktreesFixture>(f: &F) {
    let (path, _) = base_of(f, "work");
    let repo = RepoRef::new(path.as_path());
    f.commit_empty(&path);
    let empty = hash(&f.head(path.as_path()));
    let root = hash(&f.merge_unrelated_root(&path));
    let merge = hash(&f.head(path.as_path()));
    assert_eq!(f.port().patch_id(&repo, &empty), Ok(None));
    assert_eq!(f.port().patch_id(&repo, &merge), Ok(None));
    assert!(
        matches!(f.port().patch_id(&repo, &root), Ok(Some(_))),
        "a root commit has a patch"
    );
}

pub fn patch_id_of_a_commit_the_repo_lacks_is_an_error<F: WorktreesFixture>(f: &F) {
    let unknown = hash(&"deadbeef".repeat(5));
    let result = f.port().patch_id(&f.repo(), &unknown);
    assert!(result.is_err(), "{result:?}");
}

pub fn patch_id_outside_a_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let commit = hash(&f.local_default_tip());
    let result = f.port().patch_id(&f.not_a_repo(), &commit);
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

/// What `git blame` prints (an abbreviated hash) and what people type (a branch, `HEAD`) name the commit.
pub fn resolve_commit_names_hashes_abbreviations_head_and_branches<F: WorktreesFixture>(f: &F) {
    let created = f
        .port()
        .create(&f.repo(), &name("work"), BaseSpec::OriginDefault)
        .unwrap();
    f.commit(&created.path);
    let head = f.head(created.path.as_path());
    let tip = hash(&head);
    let in_worktree = RepoRef::new(created.path.as_path());
    let resolve = |repo: &RepoRef, rev: &str| f.port().resolve_commit(repo, rev);
    assert_eq!(resolve(&in_worktree, &head), Ok(Some(tip.clone())));
    assert_eq!(resolve(&in_worktree, &head[..12]), Ok(Some(tip.clone())));
    assert_eq!(resolve(&in_worktree, "HEAD"), Ok(Some(tip.clone())));
    assert_eq!(resolve(&f.repo(), created.branch.as_str()), Ok(Some(tip)));
    assert_eq!(
        resolve(&f.repo(), DEFAULT),
        Ok(Some(hash(&f.local_default_tip())))
    );
}

pub fn resolve_commit_of_a_rev_that_names_no_commit_is_none<F: WorktreesFixture>(f: &F) {
    for rev in [
        "no-such-branch".to_owned(),
        "deadbeef".repeat(5),
        "deadbeef".to_owned(),
        String::new(),
    ] {
        assert_eq!(
            f.port().resolve_commit(&f.repo(), &rev),
            Ok(None),
            "{rev:?}"
        );
    }
}

/// A rev from the command line that looks like an option is a name like any other: it names no commit, and changes
/// nothing about the call. (In the git adapter the `^{commit}` peel appended to it already keeps these from matching
/// one of git's options; `--end-of-options` is kept as defence in depth, and no case here needs it alone.)
pub fn resolve_commit_never_reads_a_rev_as_an_option<F: WorktreesFixture>(f: &F) {
    for rev in ["--all", "-h", "--git-dir=/nonexistent", "--end-of-options"] {
        assert_eq!(f.port().resolve_commit(&f.repo(), rev), Ok(None), "{rev:?}");
    }
}

pub fn resolve_commit_outside_a_repo_is_not_a_repo<F: WorktreesFixture>(f: &F) {
    let result = f.port().resolve_commit(&f.not_a_repo(), "HEAD");
    assert!(
        matches!(result, Err(WorktreeError::NotARepo(_))),
        "{result:?}"
    );
}

// ---------- filesystem ----------

/// `<tmp>/alias -> <repo>`: the repo is named by where the link leads, not by the link.
pub fn repo_root_through_a_symlink_is_the_resolved_checkout<F: FsWorktreesFixture>(f: &F) {
    let alias = f.root().with_file_name("alias");
    f.symlink(&alias, f.repo().as_path());
    let resolved = f.resolved(f.repo().as_path());
    assert_ne!(alias, resolved);
    assert_eq!(f.port().repo_root(&alias), Ok(resolved.clone()));
    assert_eq!(f.port().repo_root(&alias.join(SUBDIR)), Ok(resolved));
}

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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use shepherd_core::ports::WorktreeError;

    use super::{HeldCreate, wait_held};

    struct Entered(bool);

    impl HeldCreate for Entered {
        fn wait_entered(&self, _limit: Duration) -> bool {
            self.0
        }
    }

    #[test]
    fn a_held_create_is_held() {
        let (_tx, rx) = mpsc::channel::<Result<(), WorktreeError>>();
        assert_eq!(wait_held(&Entered(true), &rx, Duration::ZERO), Ok(()));
    }

    /// A create that fails before it gets to the hold: the failure is the create's own error, not a timeout.
    #[test]
    fn a_create_that_answers_before_it_is_held_says_what_it_answered() {
        let (tx, rx) = mpsc::channel();
        tx.send(Err::<(), _>(WorktreeError::Timeout("rev-parse".into())))
            .unwrap();
        let why = wait_held(&Entered(false), &rx, Duration::from_secs(600)).unwrap_err();
        assert!(why.contains("Timeout(\"rev-parse\")"), "{why}");
    }

    #[test]
    fn a_create_still_running_at_the_bound_says_so() {
        let (_tx, rx) = mpsc::channel::<Result<(), WorktreeError>>();
        let why = wait_held(&Entered(false), &rx, Duration::ZERO).unwrap_err();
        assert!(why.contains("still running"), "{why}");
    }
}
