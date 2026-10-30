//! Which build of shepherd this process runs, for the hello (T36a, decision 0018): the crate version and the sha256
//! digest of an executable. The bytes are read here and hashed by core's [`DigestHasher`].
//!
//! The host also says what is installed now at the path its executable was started from ([`InstalledExe`]). It
//! recomputes that digest only when the file there changes, by its `(dev, ino, size, mtime)`: installing a new build
//! (`cargo install`, a rename over the path) changes the inode, and an in-place write changes the size or mtime.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use shepherd_core::wire::{BuildId, DigestHasher, ExeDigest};

/// How the file at a path is told apart from one that replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileKey {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// The build of the executable at `exe`: this crate's version and the file's digest. A process calls it once at
/// startup, with its `current_exe()`.
pub fn build_id(exe: &Path) -> io::Result<BuildId> {
    digest_file(exe).map(|(_, digest)| build_of(digest))
}

/// The build of an executable of this crate's version whose digest is `digest`.
pub fn build_of(digest: ExeDigest) -> BuildId {
    BuildId {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        digest,
    }
}

/// The digest of the file at `path` and the key it had, both from the one open file, so a file renamed over the path
/// meanwhile can't pair one file's key with the other's bytes.
fn digest_file(path: &Path) -> io::Result<(FileKey, ExeDigest)> {
    let mut file = File::open(path)?;
    let key = FileKey::of(&file.metadata()?);
    let mut hasher = DigestHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok((key, hasher.finish()))
}

/// The digest of whatever is installed at one path now: what restarting the host would run.
#[derive(Debug, Clone)]
pub struct InstalledExe {
    path: PathBuf,
    /// The file last hashed, and its digest.
    last: Option<(FileKey, ExeDigest)>,
    computed: u64,
}

impl InstalledExe {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last: None,
            computed: 0,
        }
    }

    /// The digest of the file at the path now, or `None` if it can't be read. Hashes it only if it isn't the file
    /// hashed last.
    pub fn digest(&mut self) -> Option<ExeDigest> {
        let key = FileKey::of(&std::fs::metadata(&self.path).ok()?);
        match self.last {
            Some((last, digest)) if last == key => Some(digest),
            _ => {
                self.computed += 1;
                match digest_file(&self.path) {
                    Ok(hashed) => {
                        self.last = Some(hashed);
                        Some(hashed.1)
                    }
                    Err(_) => {
                        self.last = None;
                        None
                    }
                }
            }
        }
    }

    /// How many times the file has been hashed.
    pub fn digests_computed(&self) -> u64 {
        self.computed
    }
}
