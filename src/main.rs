mod core;
mod config;
mod history;
mod pty_widget;
mod session;
mod session_manager;
mod status_socket;
mod worktree;

use session_manager::TuiSessionManager;

fn main() -> anyhow::Result<()> {
    let mut manager = TuiSessionManager::new()?;

    // Try to resume a previous session, otherwise open new session dialog
    if !manager.try_resume()? {
        manager.open_new_session();
    }

    manager.run()?;

    Ok(())
}
