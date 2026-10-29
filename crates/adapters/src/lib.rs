//! Real implementations of the ports defined in `shepherd_core::ports`.
//!
//! Adapters are thin: translate domain calls to IO and back. No policy.

pub mod config;
pub mod event_log;
pub mod link;
pub mod private_fs;
pub mod pty;
pub mod worktrees;

pub use config::TomlConfigSource;
pub use event_log::{JsonlEventLog, TornTail};
pub use link::{UnixConnectLink, UnixLinkError, UnixListenLink};
pub use pty::PortablePty;
pub use worktrees::GitWorktrees;
