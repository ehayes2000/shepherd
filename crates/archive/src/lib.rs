//! The conversation archive (decision 0020): agent transcripts kept on this device, content-addressed, compressed and
//! private, with a SQLite index from repos, sessions and commits to them. An independent crate: it depends on no
//! internal crate, reads only the paths it is given, and never reads a clock, the environment or the network.

mod blob;
mod ids;
mod index;
mod private;

pub use blob::{BlobError, BlobId, BlobStore, DEFAULT_MAX_BLOB_LEN, Sweep};
#[cfg(feature = "fault-injection-crash-before-rename")]
pub use blob::{CRASH_BEFORE_RENAME, CRASH_EXIT_CODE};
#[cfg(feature = "fault-injection-pause-before-rename")]
pub use blob::{PAUSE_BEFORE_RENAME, PAUSE_MARKER};
pub use ids::{
    AgentKind, AgentSessionId, BadValue, Branch, CommitHash, FileRole, MachineId, PatchId,
    PrNumber, RepoId, SessionId, SnapshotId, SnapshotReason,
};
pub use index::{
    Archive, ArchiveError, BadRow, CommitHit, INDEX_VERSION, NewFile, NewSnapshot, ReadFile,
    Session, SessionCommit, Snapshot, SnapshotFile, VerifyReport,
};
