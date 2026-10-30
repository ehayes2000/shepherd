//! Managed git worktrees: one per session, under a managed root (`~/.shepherd/worktrees` in production).
//!
//! A managed worktree lives at `<root>/<repo-name>/<name>` and is checked out on a new branch called `<name>`.
//! `<repo-name>` comes from the repo's common git dir (see [`repo_name_from_common_dir`]), so every checkout of one
//! repo (its main checkout and each of its worktrees) maps to the same directory.
//!
//! The trait is synchronous. Every git call is bounded by a timeout; a fetch that times out counts as a failed fetch,
//! anything else that times out is [`WorktreeError::Timeout`].
//!
//! # Terms
//!
//! - **default branch**: the branch `origin/HEAD` points at, e.g. `main`. Not guessed. If the repo has no
//!   `origin/HEAD` (e.g. it was made with `git init` + `git remote add` rather than cloned), the port asks origin for
//!   its HEAD once (`git remote set-head origin --auto`, which needs the network) and writes that one ref, as
//!   `git clone` would have. Only if that fails is it [`WorktreeError::NoDefaultBranch`].
//! - **origin default**: the remote-tracking ref `origin/<default>` as of the last fetch. [`Worktrees::status`] never
//!   fetches.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Any checkout of a repo: its main checkout or one of its worktrees. All of them name the same repo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoRef(PathBuf);

/// The directory of a worktree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorktreePath(PathBuf);

/// A local branch, without the `refs/heads/` prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

/// The name of a managed worktree (a session slug). It names both the worktree's directory and its branch, so it is
/// restricted to what is safe as both: 1 to 64 characters of `[A-Za-z0-9._-]`, starting with a letter or digit, with no
/// `..`, not ending in `.` or `.lock`, and not `HEAD`. Within that character set, these are git's branch name rules
/// (`git check-ref-format --branch`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorktreeName(String);

macro_rules! path_newtype {
    ($t:ident) => {
        impl $t {
            pub fn new(path: impl Into<PathBuf>) -> Self {
                Self(path.into())
            }

            pub fn as_path(&self) -> &Path {
                &self.0
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.display(), f)
            }
        }
    };
}

path_newtype!(RepoRef);
path_newtype!(WorktreePath);

impl BranchName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl WorktreeName {
    pub fn new(name: impl Into<String>) -> Result<Self, WorktreeError> {
        let name = name.into();
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let valid = (1..=64).contains(&name.len())
            && name.chars().all(allowed)
            && name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && !name.contains("..")
            && !name.ends_with('.')
            && !name.ends_with(".lock")
            && name != "HEAD";
        if valid {
            Ok(Self(name))
        } else {
            Err(WorktreeError::InvalidName(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The branch a worktree with this name is checked out on.
    pub fn branch(&self) -> BranchName {
        BranchName(self.0.clone())
    }
}

impl fmt::Display for WorktreeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a new worktree's branch starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSpec {
    /// Fetch origin, then start from `origin/<default>`. If the fetch fails (offline, timeout), start from
    /// `origin/<default>` as last fetched, or if there is none from the local `<default>`, and say so in
    /// [`Created::base`].
    OriginDefault,
}

/// What a new worktree's branch actually started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUsed {
    /// `origin/<branch>`, freshly fetched.
    Origin { branch: BranchName },
    /// The fetch failed, so `origin/<branch>` as last fetched was used. It may be behind origin.
    Stale {
        branch: BranchName,
        fetch_error: String,
    },
    /// The fetch failed and there is no `origin/<branch>` at all, so the local `<branch>` was used.
    Local {
        branch: BranchName,
        fetch_error: String,
    },
}

/// A worktree that [`Worktrees::create`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub path: WorktreePath,
    pub branch: BranchName,
    pub base: BaseUsed,
}

/// A managed worktree as [`Worktrees::list_managed`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: WorktreePath,
    /// `None` when its HEAD is detached.
    pub branch: Option<BranchName>,
}

/// What would be lost by removing a worktree. Computed from local refs only: nothing is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct WorktreeStatus {
    /// Uncommitted changes: staged, unstaged, or untracked files that aren't ignored.
    pub dirty: bool,
    /// Commits on HEAD that aren't reachable from the origin default.
    pub ahead: u32,
    /// HEAD is on a branch with an upstream that still exists, and HEAD has no commits the upstream lacks.
    pub pushed: bool,
    /// HEAD is reachable from the origin default. Squash and rebase merges are not detected. A branch with no commits
    /// of its own counts as merged.
    pub merged_into_default: bool,
}

impl WorktreeStatus {
    /// What removing the worktree would lose: its uncommitted changes, and its commits if they are neither merged into
    /// the origin default nor pushed. Empty when removing it loses nothing.
    pub fn losses(&self) -> Vec<Loss> {
        let mut losses = Vec::new();
        if self.dirty {
            losses.push(Loss::UncommittedChanges);
        }
        if !self.merged_into_default && !self.pushed {
            losses.push(Loss::Unpushed {
                commits: self.ahead,
            });
        }
        losses
    }
}

/// Something removing a worktree would lose, see [`WorktreeStatus::losses`]. Its text is for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    UncommittedChanges,
    /// Commits that are neither merged into the origin default nor pushed.
    Unpushed {
        commits: u32,
    },
}

impl fmt::Display for Loss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UncommittedChanges => f.write_str("uncommitted changes"),
            Self::Unpushed { commits: 1 } => f.write_str("1 commit not pushed or merged"),
            Self::Unpushed { commits } => write!(f, "{commits} commits not pushed or merged"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    /// The path is not inside a git checkout, or does not exist.
    #[error("{0} is not a git repository")]
    NotARepo(PathBuf),
    /// The repo has no `origin/HEAD` and origin could not be asked for it, or `origin/<default>` doesn't exist where
    /// it is needed (status).
    #[error("{0} has no origin default branch")]
    NoDefaultBranch(PathBuf),
    /// A local branch with this name already exists. Removing a worktree keeps its branch, so a name stays taken.
    #[error("branch {0} already exists")]
    BranchExists(BranchName),
    /// The worktree has uncommitted changes and removal was not forced.
    #[error("{0} has uncommitted changes")]
    Dirty(WorktreePath),
    /// The fetch failed and there was neither `origin/<default>` nor a local default branch to fall back to.
    #[error("fetching origin failed: {0}")]
    FetchFailed(String),
    /// `<root>/<repo-name>/` already holds worktrees of a different repo with the same directory name.
    #[error("{path} already holds worktrees of another repo with the same name")]
    RepoNameTaken { path: PathBuf },
    /// Not a valid [`WorktreeName`].
    #[error("{0:?} is not a valid worktree name")]
    InvalidName(String),
    /// Removal refused: the path is not a worktree directory directly under `<root>/<repo-name>/`, after resolving
    /// `..` and symlinks.
    #[error("{0} is not inside the managed worktree root")]
    OutsideManagedRoot(PathBuf),
    /// A git call other than fetch ran past its timeout.
    #[error("git {0} timed out")]
    Timeout(String),
    #[error("{0}")]
    Other(String),
}

pub trait Worktrees {
    /// The repo `path` is in, named by its main checkout's top-level directory with symlinks resolved. It is the same
    /// for the main checkout, any directory inside it, and any linked worktree of it (found through the common git
    /// dir). A repo with no main checkout to name (a worktree of a bare repo, a submodule) is named by the top level of
    /// the checkout `path` is in. A path outside any checkout, or one that doesn't exist, is [`WorktreeError::NotARepo`].
    fn repo_root(&self, path: &Path) -> Result<PathBuf, WorktreeError>;

    /// The repo's default branch (see the module docs). May write `origin/HEAD` if it is missing.
    fn default_branch(&self, repo: &RepoRef) -> Result<BranchName, WorktreeError>;

    /// Creates `<root>/<repo-name>/<name>` on a new branch `<name>` started from `base`. The new branch has no upstream
    /// until it is pushed. [`WorktreeError::RepoNameTaken`] if another repo with the same name already has worktrees
    /// under the root.
    ///
    /// If making the worktree times out (e.g. a slow `post-checkout` hook), the half-made worktree and its new branch
    /// are removed before returning [`WorktreeError::Timeout`], so a retry with the same name can succeed. If that
    /// cleanup fails too, the error is [`WorktreeError::Other`], saying the create timed out, why cleanup failed, and
    /// that the directory and branch must be removed by hand. That includes finding a directory at the worktree's path
    /// that git doesn't list as a worktree: it may not be ours, so it is left alone.
    fn create(
        &self,
        repo: &RepoRef,
        name: &WorktreeName,
        base: BaseSpec,
    ) -> Result<Created, WorktreeError>;

    /// The repo's worktrees under `<root>/<repo-name>/`, at the same paths [`Worktrees::create`] returned. The main
    /// checkout and worktrees elsewhere are not listed.
    fn list_managed(&self, repo: &RepoRef) -> Result<Vec<Worktree>, WorktreeError>;

    /// See [`WorktreeStatus`]. Works on any checkout, managed or not. A path that doesn't exist is
    /// [`WorktreeError::NotARepo`].
    fn status(&self, path: &WorktreePath) -> Result<WorktreeStatus, WorktreeError>;

    /// Removes a managed worktree's directory and git's record of it. Its branch is kept, with any commits on it:
    /// deleting branches is the caller's decision. A dirty worktree is [`WorktreeError::Dirty`] unless `force`.
    /// Anything not directly under `<root>/<repo-name>/` is [`WorktreeError::OutsideManagedRoot`], whatever `force`
    /// says. A managed-looking path that doesn't exist is [`WorktreeError::NotARepo`].
    fn remove(&self, path: &WorktreePath, force: bool) -> Result<(), WorktreeError>;
}

/// The repo name used under the managed root, from the absolute path `git rev-parse --git-common-dir` prints: the
/// checkout's directory name for `<checkout>/.git`, or the directory name minus `.git` for a bare repo.
pub fn repo_name_from_common_dir(common_dir: &Path) -> Option<String> {
    let name = common_dir.file_name()?.to_str()?;
    let name = if name == ".git" {
        common_dir.parent()?.file_name()?.to_str()?
    } else {
        name.strip_suffix(".git").unwrap_or(name)
    };
    (!name.is_empty()).then(|| name.to_owned())
}

/// `path` with `.` and `..` resolved lexically, without touching the filesystem. `None` if `..` climbs above the start
/// of a relative path or above `/`.
pub fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    return None;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// If `path` is exactly `<root>/<repo>/<name>`, returns `(repo, name)`. Both paths are compared as given, so resolve
/// `..` and symlinks first.
pub fn managed_parts<'a>(root: &Path, path: &'a Path) -> Option<(&'a str, &'a str)> {
    let rest = path.strip_prefix(root).ok()?;
    let mut parts = rest.components();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Component::Normal(repo)), Some(Component::Normal(name)), None) => {
            Some((repo.to_str()?, name.to_str()?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_names_accept_slugs() {
        for name in [
            "fix-login",
            "a",
            "T20.worktrees",
            "s_01abc",
            &"x".repeat(64),
        ] {
            assert!(WorktreeName::new(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn worktree_names_reject_paths_and_bad_refs() {
        for name in [
            "",
            "../x",
            "a/b",
            ".hidden",
            "-flag",
            "a..b",
            "trailing.",
            "x.lock",
            "HEAD",
            "sp ace",
            "ünï",
            &"x".repeat(65),
        ] {
            assert_eq!(
                WorktreeName::new(name),
                Err(WorktreeError::InvalidName(name.to_owned())),
                "{name}"
            );
        }
    }

    #[test]
    fn repo_name_of_a_checkout_is_its_directory() {
        let name = repo_name_from_common_dir(Path::new("/home/me/code/api/.git"));
        assert_eq!(name.as_deref(), Some("api"));
    }

    #[test]
    fn repo_name_of_a_bare_repo_drops_dot_git() {
        let name = repo_name_from_common_dir(Path::new("/srv/api.git"));
        assert_eq!(name.as_deref(), Some("api"));
        let name = repo_name_from_common_dir(Path::new("/srv/plain"));
        assert_eq!(name.as_deref(), Some("plain"));
    }

    #[test]
    fn normalize_resolves_dots_and_refuses_climbing_out() {
        assert_eq!(
            normalize_lexically(Path::new("/r/a/./b/../c")),
            Some(PathBuf::from("/r/a/c"))
        );
        assert_eq!(
            normalize_lexically(Path::new("/r/a/../../x")),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(normalize_lexically(Path::new("/..")), None);
        assert_eq!(normalize_lexically(Path::new("a/../..")), None);
    }

    #[test]
    fn managed_parts_needs_exactly_repo_and_name_under_root() {
        let root = Path::new("/root");
        assert_eq!(
            managed_parts(root, Path::new("/root/api/fix")),
            Some(("api", "fix"))
        );
        assert_eq!(managed_parts(root, Path::new("/root")), None);
        assert_eq!(managed_parts(root, Path::new("/root/api")), None);
        assert_eq!(managed_parts(root, Path::new("/root/api/fix/src")), None);
        assert_eq!(managed_parts(root, Path::new("/rootx/api/fix")), None);
        assert_eq!(managed_parts(root, Path::new("/elsewhere/api/fix")), None);
    }
}
