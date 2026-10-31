//! The conversation archive (decision 0020): agent transcripts kept on this device, content-addressed, compressed and
//! private. An independent crate: it depends on no internal crate, reads only the paths it is given, and never reads
//! a clock, the environment or the network.

mod blob;
mod private;

pub use blob::{BlobError, BlobId, BlobStore, DEFAULT_MAX_BLOB_LEN};
#[cfg(feature = "fault-injection-crash-before-rename")]
pub use blob::{CRASH_BEFORE_RENAME, CRASH_EXIT_CODE};
