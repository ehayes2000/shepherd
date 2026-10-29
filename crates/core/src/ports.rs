//! Port traits: every capability that touches the world. Implementations live in `shepherd-adapters`,
//! fakes and contract suites in `shepherd-testkit`. See the `new-port` skill.

pub mod pty;

pub use pty::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};
