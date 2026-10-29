//! Port traits: every capability that touches the world. Implementations live in `shepherd-adapters`,
//! fakes and contract suites in `shepherd-testkit`. See the `new-port` skill.

pub mod config;
pub mod event_log;
pub mod pty;

pub use config::{ConfigError, ConfigSource, RawConfig};
pub use event_log::{EventLog, LogError, LogId};
pub use pty::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};
