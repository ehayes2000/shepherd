//! Real implementations of the ports defined in `shepherd_core::ports`.
//!
//! Adapters are thin: translate domain calls to IO and back. No policy.

pub mod commands;
pub mod config;
pub mod event_log;
pub mod forge;
pub mod handoff;
pub mod hostname;
pub mod identity;
pub mod iroh_link;
pub mod launcher;
pub mod link;
pub mod peers;
pub mod private_fs;
mod process_group;
pub mod pty;
#[cfg(test)]
mod stand_in;
#[cfg(test)]
mod test_pids;
pub mod worktrees;

pub use commands::ShellCommands;
pub use config::TomlConfigSource;
pub use event_log::{JsonlEventLog, TornTail};
pub use forge::GhForge;
pub use identity::{IrohMachineKey, KeyFileIdentity};
pub use iroh_link::IrohLink;
pub use launcher::ProcessHostLauncher;
pub use link::{UnixConnectLink, UnixLinkError, UnixListenLink};
pub use peers::{Discovery, IrohPeers};
pub use pty::{PortablePty, PtyToken};
pub use worktrees::GitWorktrees;
