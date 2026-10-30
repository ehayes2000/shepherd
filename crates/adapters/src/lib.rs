//! Real implementations of the ports defined in `shepherd_core::ports`.
//!
//! Adapters are thin: translate domain calls to IO and back. No policy.

pub mod commands;
pub mod config;
pub mod event_log;
pub mod launcher;
pub mod link;
pub mod private_fs;
mod process_group;
pub mod pty;
pub mod worktrees;

pub use commands::ShellCommands;
pub use config::TomlConfigSource;
pub use event_log::{JsonlEventLog, TornTail};
pub use launcher::ProcessHostLauncher;
pub use link::{UnixConnectLink, UnixLinkError, UnixListenLink};
pub use pty::PortablePty;
pub use worktrees::GitWorktrees;
