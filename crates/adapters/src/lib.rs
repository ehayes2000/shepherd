//! Real implementations of the ports defined in `shepherd_core::ports`.
//!
//! Adapters are thin: translate domain calls to IO and back. No policy.

pub mod config;
pub mod event_log;
pub mod pty;

pub use config::TomlConfigSource;
pub use event_log::{JsonlEventLog, TornTail};
pub use pty::PortablePty;
