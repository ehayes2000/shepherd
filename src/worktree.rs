use crate::config::Config;
use crate::session_manager::StatusMessage;
use std::process::Command;

/// Workflow that creates git worktrees for each session
pub struct WorktreeWorkflow;

impl WorktreeWorkflow {
    const NAME: &'static str = "worktree";

    fn error(log_message: impl Into<String>) -> StatusMessage {
        StatusMessage::err(format!("Workflow {} failed", Self::NAME), log_message)
    }

    /// Get the repository name from the current directory
    fn get_repo_name() -> Result<String, StatusMessage> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| Self::error(format!("failed to run git rev-parse: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Self::error(format!(
                "not in a git repository: {}",
                stderr.trim()
            )));
        }

        let repo_path = String::from_utf8(output.stdout)
            .map_err(|e| Self::error(format!("invalid utf8 in git output: {}", e)))?
            .trim()
            .to_string();

        let repo_name = std::path::Path::new(&repo_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Self::error("could not determine repository name from path"))?
            .to_string();

        Ok(repo_name)
    }

    /// Get the main branch name (main or master)
    fn get_main_branch() -> Result<String, StatusMessage> {
        // Check if 'main' exists
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "main"])
            .output()
            .map_err(|e| Self::error(format!("failed to run git rev-parse: {}", e)))?;

        if output.status.success() {
            return Ok("main".to_string());
        }

        // Fall back to 'master'
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "master"])
            .output()
            .map_err(|e| Self::error(format!("failed to run git rev-parse: {}", e)))?;

        if output.status.success() {
            return Ok("master".to_string());
        }

        Err(Self::error("could not find main or master branch"))
    }
}

impl WorktreeWorkflow {
    pub fn pre_session_hook(
        session_name: &str,
        config: &Config,
    ) -> Result<SessionMetadata, StatusMessage> {
        let repo_name = Self::get_repo_name()?;
        let main_branch = Self::get_main_branch()?;

        // Build worktree path: <workflows_path>/<reponame>/<sessionname>
        let worktree_path = config.workflows_path.join(&repo_name).join(session_name);

        // Fetch latest from origin
        let output = Command::new("git")
            .args(["fetch", "origin", &main_branch])
            .output()
            .map_err(|e| Self::error(format!("failed to run git fetch: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Self::error(format!(
                "git fetch origin {} failed: {}",
                main_branch,
                stderr.trim()
            )));
        }

        // Create the worktree with a new branch based on origin/main
        let worktree_path_str = worktree_path
            .to_str()
            .ok_or_else(|| Self::error("worktree path contains invalid UTF-8"))?;

        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                session_name,
                worktree_path_str,
                &format!("origin/{}", main_branch),
            ])
            .output()
            .map_err(|e| Self::error(format!("failed to run git worktree add: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Self::error(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }

        Ok(SessionMetadata { path: worktree_path })
    }
}

#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub path: std::path::PathBuf,
}
