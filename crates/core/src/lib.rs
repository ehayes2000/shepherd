//! Shepherd's functional core: domain state, decisions, the ACP++ model, and port traits.
//!
//! Everything here is pure. See `docs/architecture.md`.

pub mod acpp;
pub mod ids;
pub mod ports;
pub mod session;
pub mod wire;
