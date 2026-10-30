//! Port traits: every capability that touches the world. Implementations live in `shepherd-adapters`,
//! fakes and contract suites in `shepherd-testkit`. See the `new-port` skill.

pub mod commands;
pub mod config;
pub mod event_log;
pub mod launcher;
pub mod link;
pub mod pty;
pub mod worktrees;

pub use commands::{CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL};
pub use config::{ConfigError, ConfigSource, RawConfig};
pub use event_log::{EventLog, LogError, LogId};
pub use launcher::{HostLauncher, LaunchError};
pub use link::{ConnId, DisconnectReason, Link, LinkError, LinkEvent, LinkEventSink, MAX_QUEUED};
pub use pty::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec,
};
pub use worktrees::{
    BaseSpec, BaseUsed, BranchName, Created, Loss, RepoRef, Worktree, WorktreeError, WorktreeName,
    WorktreePath, WorktreeStatus, Worktrees,
};
