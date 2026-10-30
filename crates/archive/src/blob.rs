//! Content-addressed blobs: `<root>/blobs/<first 2 hex>/<sha256 hex>.zst`, the zstd of the bytes whose sha256 names
//! the file. Writes are atomic (temp file in the same dir, fsync, rename, fsync dir), so a blob is either absent or
//! complete under its name. Reads verify the hash and are bounded, so a corrupt or hostile file is an error, never
//! wrong bytes or an unbounded allocation.

use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::private::{
    create_new_private_file, ensure_private_dir, io_err, normalize, open_existing_private_file,
    sync_dir,
};

/// The largest blob [`BlobStore::put`] accepts and [`BlobStore::get`] inflates, unless set with
/// [`BlobStore::with_max_len`]. Transcripts are text and rarely reach tens of megabytes.
pub const DEFAULT_MAX_BLOB_LEN: u64 = 256 << 20;

/// zstd's default level: fast, and transcripts (JSON lines) compress well at it.
const ZSTD_LEVEL: i32 = 3;

/// With the `fault-injection-crash-before-rename` feature (tests only, decision 0015), [`BlobStore::put`] of content
/// starting with these bytes exits the process with [`CRASH_EXIT_CODE`] after the temp file is written and synced,
/// before it is renamed into place.
#[cfg(feature = "fault-injection-crash-before-rename")]
pub const CRASH_BEFORE_RENAME: &[u8] = b"\0shepherd-archive-test-crash-before-rename\0";
#[cfg(feature = "fault-injection-crash-before-rename")]
pub const CRASH_EXIT_CODE: i32 = 86;

/// Names temp files uniquely within this process; the pid separates processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A blob's name: the sha256 of its uncompressed bytes. Its text form is 64 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId([u8; 32]);

impl BlobId {
    /// The id `bytes` are stored under.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// Text that isn't 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a blob id (64 lowercase hex digits)")]
pub struct BadBlobId(String);

impl FromStr for BlobId {
    type Err = BadBlobId;

    fn from_str(s: &str) -> Result<Self, BadBlobId> {
        let bad = || BadBlobId(s.to_owned());
        // One spelling per id: uppercase would parse, but would name a different file.
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (byte, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Self(out))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    /// The path exists and group or others have some access to it. It is never chmodded: the user fixes it.
    #[error("{path} has mode {mode:o}; it must not be accessible to group or others")]
    Insecure { path: PathBuf, mode: u32 },
    /// The final part of the path is a symlink (or was swapped for something else while it was opened).
    #[error("{0} is a symlink; refusing to follow it")]
    Symlink(PathBuf),
    /// A directory was wanted and something else is there, or the other way round.
    #[error("{0} exists and is the wrong kind of file")]
    WrongType(PathBuf),
    #[error("blob {id} is not in the archive (no {path})")]
    Missing { id: BlobId, path: PathBuf },
    /// The file does not decompress, or decompresses to bytes whose sha256 isn't `id`.
    #[error("blob {id} at {path} is corrupt: {reason}")]
    Corrupt {
        id: BlobId,
        path: PathBuf,
        reason: String,
    },
    /// The blob is, or inflates to, more than the store's cap.
    #[error("blob {id} is larger than the {max}-byte cap")]
    TooLarge { id: BlobId, max: u64 },
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// The blob store under an archive root.
#[derive(Debug)]
pub struct BlobStore {
    blobs: PathBuf,
    max_len: u64,
    writes: AtomicU64,
    inflated: AtomicU64,
}

/// Counts the bytes read through it into `total`.
struct Counting<'a, R> {
    inner: R,
    total: &'a AtomicU64,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl BlobStore {
    /// Opens the store under `root`, creating `root` and `root/blobs` exactly `0700` if they are missing. An existing
    /// `root` or `blobs` that is a symlink, not a directory, or accessible to group or others is refused and left alone.
    pub fn open(root: &Path) -> Result<Self, BlobError> {
        let root = normalize(root);
        ensure_private_dir(&root)?;
        let blobs = root.join("blobs");
        ensure_private_dir(&blobs)?;
        Ok(Self {
            blobs,
            max_len: DEFAULT_MAX_BLOB_LEN,
            writes: AtomicU64::new(0),
            inflated: AtomicU64::new(0),
        })
    }

    /// Sets the largest blob this store writes or reads, in uncompressed bytes.
    pub fn with_max_len(self, max_len: u64) -> Self {
        Self { max_len, ..self }
    }

    /// How many blob files this handle has written. A put of content already stored writes none.
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// How many decompressed bytes this handle has produced while reading blobs, including blobs it then rejected.
    /// Reading stops one byte past the cap, so a blob that inflates without bound adds at most `max_len + 1`.
    pub fn inflated(&self) -> u64 {
        self.inflated.load(Ordering::Relaxed)
    }

    fn shard_dir(&self, id: &BlobId) -> PathBuf {
        self.blobs.join(format!("{:02x}", id.0[0]))
    }

    fn path(&self, id: &BlobId) -> PathBuf {
        self.shard_dir(id).join(format!("{id}.zst"))
    }

    /// Stores `bytes` under their sha256 and returns it. Content already stored intact is not written again; a
    /// stored copy that is corrupt or truncated is replaced.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobId, BlobError> {
        let id = BlobId::of(bytes);
        if bytes.len() as u64 > self.max_len {
            return Err(BlobError::TooLarge {
                id,
                max: self.max_len,
            });
        }
        let shard = self.shard_dir(&id);
        ensure_private_dir(&shard)?;
        let path = self.path(&id);
        // Already stored only if it reads back right: a corrupt or truncated blob is replaced, so a successful put
        // always means `get` returns these bytes. A blob that is insecure, a symlink or not a file is refused as
        // `get` would refuse it, and left alone.
        match self.get(&id) {
            Ok(_) => return Ok(id),
            Err(
                BlobError::Missing { .. } | BlobError::Corrupt { .. } | BlobError::TooLarge { .. },
            ) => {}
            Err(e) => return Err(e),
        }

        let compressed = zstd::encode_all(bytes, ZSTD_LEVEL).map_err(|e| io_err(&path, e))?;
        let temp = self.write_temp(&shard, &id, &compressed)?;

        #[cfg(feature = "fault-injection-crash-before-rename")]
        if bytes.starts_with(CRASH_BEFORE_RENAME) {
            std::process::exit(CRASH_EXIT_CODE);
        }

        // Same content under the same name, so a concurrent writer that got there first is harmless.
        if let Err(e) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(io_err(&path, e));
        }
        sync_dir(&shard)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }

    /// Writes `body` to a new `0600` temp file in `dir` and syncs it. Temp names start with `.`, so they never
    /// look like a blob.
    fn write_temp(&self, dir: &Path, id: &BlobId, body: &[u8]) -> Result<PathBuf, BlobError> {
        loop {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp = dir.join(format!(".{id}.{}.{n}.tmp", std::process::id()));
            let mut file = match create_new_private_file(&temp) {
                Ok(file) => file,
                // A stale temp from a crashed process with the same pid: pick another name.
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_err(&temp, e)),
            };
            if let Err(e) = file.write_all(body).and_then(|()| file.sync_all()) {
                let _ = fs::remove_file(&temp);
                return Err(io_err(&temp, e));
            }
            return Ok(temp);
        }
    }

    /// The bytes stored under `id`, decompressed and checked against it.
    pub fn get(&self, id: &BlobId) -> Result<Vec<u8>, BlobError> {
        let path = self.path(id);
        if let Err(e) = fs::symlink_metadata(&path) {
            return Err(if e.kind() == ErrorKind::NotFound {
                BlobError::Missing {
                    id: *id,
                    path: path.clone(),
                }
            } else {
                io_err(&path, e)
            });
        }
        let file = open_existing_private_file(&path)?;
        let corrupt = |reason: String| BlobError::Corrupt {
            id: *id,
            path: path.clone(),
            reason,
        };

        let mut file = BufReader::new(file);
        // Reserve what the frame header says it holds, but never more than the cap: the header is the file's word.
        let declared = file.fill_buf().map_err(|e| io_err(&path, e))?;
        let reserve = zstd::zstd_safe::get_frame_content_size(declared)
            .ok()
            .flatten()
            .map_or(0, |n| n.min(self.max_len));
        let mut bytes = Vec::with_capacity(usize::try_from(reserve).unwrap_or(0));
        let decoder =
            zstd::stream::read::Decoder::with_buffer(file).map_err(|e| io_err(&path, e))?;
        // Read at most one byte past the cap: enough to tell "too large" without inflating the rest.
        Counting {
            inner: decoder,
            total: &self.inflated,
        }
        .take(self.max_len.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| corrupt(e.to_string()))?;
        if bytes.len() as u64 > self.max_len {
            return Err(BlobError::TooLarge {
                id: *id,
                max: self.max_len,
            });
        }
        if BlobId::of(&bytes) != *id {
            return Err(corrupt("its content does not match its sha256".to_owned()));
        }
        Ok(bytes)
    }

    /// Every blob id in the store, sorted. Temp files, stray files and anything not named like a blob in its own
    /// shard are not blobs and are skipped.
    pub fn list(&self) -> Result<Vec<BlobId>, BlobError> {
        let mut ids = Vec::new();
        for shard in read_dir(&self.blobs)? {
            let shard = shard.map_err(|e| io_err(&self.blobs, e))?;
            let is_dir = shard
                .file_type()
                .map_err(|e| io_err(&shard.path(), e))?
                .is_dir();
            if !is_dir {
                continue;
            }
            for entry in read_dir(&shard.path())? {
                let entry = entry.map_err(|e| io_err(&shard.path(), e))?;
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".zst"))
                    .and_then(|hex| hex.parse::<BlobId>().ok())
                else {
                    continue;
                };
                let in_its_shard = shard.path() == self.shard_dir(&id);
                let is_file = entry
                    .file_type()
                    .map_err(|e| io_err(&entry.path(), e))?
                    .is_file();
                if in_its_shard && is_file {
                    ids.push(id);
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

fn read_dir(dir: &Path) -> Result<fs::ReadDir, BlobError> {
    fs::read_dir(dir).map_err(|e| io_err(dir, e))
}
