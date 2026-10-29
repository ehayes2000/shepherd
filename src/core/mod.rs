use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::history::SessionHistory;
use crate::status_socket::StatusSocket;

pub struct Core {
    pub config: Config,
    pub history: SessionHistory,
    pub status_socket: Option<StatusSocket>,
    pub startup_path: PathBuf,
}

impl Core {
    pub fn new() -> anyhow::Result<Self> {
        let config = Config::load()?;
        let startup_path = std::env::current_dir()?;
        let history = SessionHistory::load().unwrap_or_default();
        let status_socket = StatusSocket::new().ok();

        Ok(Self {
            config,
            history,
            status_socket,
            startup_path,
        })
    }

    /// Get the current repository name from git.
    pub fn get_current_repo_name(&self) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(&self.startup_path)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let git_common_dir = String::from_utf8(output.stdout).ok()?.trim().to_string();
        let git_path = std::path::Path::new(&git_common_dir);

        let absolute_git_path = if git_path.is_relative() {
            self.startup_path.join(git_path)
        } else {
            git_path.to_path_buf()
        };

        absolute_git_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    }

    /// Get the current repository root path from git.
    pub fn get_current_project_path(&self) -> Option<PathBuf> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&self.startup_path)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let repo_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        Some(PathBuf::from(repo_path))
    }

    /// Compute the worktree path for a given repo name and session name.
    pub fn worktree_path(&self, repo_name: &str, session_name: &str) -> PathBuf {
        self.config
            .workflows_path
            .join(repo_name)
            .join(session_name)
    }

    /// List worktree directories for the current repo.
    /// Worktrees are stored at <workflows_path>/<reponame>/<feature-name>.
    pub fn list_worktree_dirs(&self) -> Vec<PathBuf> {
        let Some(repo_name) = self.get_current_repo_name() else {
            return Vec::new();
        };

        let repo_worktrees_path = self.config.workflows_path.join(&repo_name);

        if !repo_worktrees_path.exists() {
            return Vec::new();
        }

        let Ok(entries) = std::fs::read_dir(&repo_worktrees_path) else {
            return Vec::new();
        };

        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();

        dirs.sort();
        dirs
    }

    /// Delete a single worktree (git worktree remove + directory cleanup)
    pub fn delete_worktree(&self, worktree_path: &Path) -> anyhow::Result<()> {
        let worktree_str = worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid path"))?;

        let output = std::process::Command::new("git")
            .args(["worktree", "remove", worktree_str])
            .current_dir(&self.startup_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "git worktree remove failed: {}",
                stderr.trim()
            ));
        }

        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path)?;
        }

        Ok(())
    }
}
