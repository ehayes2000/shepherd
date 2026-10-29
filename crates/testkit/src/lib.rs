//! Test support shared by every crate: fakes, contract suites, and builders.
//!
//! Depend on this crate only under `[dev-dependencies]`.

/// Builders for domain values (`AppBuilder`, `SessionBuilder`, ...), so tests don't construct large structs inline.
pub mod builders {}

/// One shared suite per port: `pub fn suite<P: Port>(make: impl Fn(..) -> P)`, run against the fake and the real
/// adapter. `make` takes whatever the port is constructed with (e.g. an event sink).
pub mod contracts {
    pub mod pty;
}

/// One stateful in-memory fake per port. Each must pass its port's contract suite.
pub mod fakes {
    pub mod pty;
}
