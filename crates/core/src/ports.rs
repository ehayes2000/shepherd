//! Port traits: every capability that touches the world. Implementations live in `shepherd-adapters`,
//! fakes and contract suites in `shepherd-testkit`. See the `new-port` skill.

pub mod archive;
pub mod commands;
pub mod config;
pub mod event_log;
pub mod forge;
pub mod identity;
pub mod launcher;
pub mod link;
pub mod peers;
pub mod pty;
pub mod worktrees;

pub use archive::{Archive, ArchiveError, ArchivedSession, CaptureReason, CaptureRequest};
pub use commands::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL, RunId,
};
pub use config::{ConfigError, ConfigSource, RawConfig};
pub use event_log::{EventLog, LogError, LogId};
pub use forge::{Check, Forge, ForgeError, Pr, PrChecks, PrState};
pub use identity::{IdentityError, MachineIdentity, MachineKey};
pub use launcher::{HostLauncher, LaunchError, TerminateError};
pub use link::{
    CLOSE_FLUSH, ConnId, DisconnectReason, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink,
    MAX_QUEUED,
};
pub use peers::{
    ConnectError, JoinError, PairTicket, PairToken, PairingError, PairingId, Peers, PeersEvent,
    PeersEventSink, TOKEN_LEN,
};
pub use pty::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtyHandoff, PtySize,
    SpawnSpec,
};
pub use worktrees::{
    BadHash, BaseSpec, BaseUsed, BranchName, CommitHash, Created, Loss, PatchId, RepoRef, RepoRoot,
    Worktree, WorktreeError, WorktreeName, WorktreePath, WorktreeStatus, Worktrees,
};
