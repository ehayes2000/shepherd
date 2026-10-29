//! Real implementations of the ports defined in `shepherd_core::ports`.
//!
//! Adapters are thin: translate domain calls to IO and back. No policy.

pub mod pty;

pub use pty::PortablePty;
