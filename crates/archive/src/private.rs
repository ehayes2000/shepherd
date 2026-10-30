//! The archive's private directories and files (decision 0010, 0020): the same rules as `shepherd-adapters`'
//! `private_fs`, reimplemented here because this crate depends on no internal crate. Only std is used, so no new
//! dependency is needed; the descriptor-based checks `private_fs` does with `O_NOFOLLOW` are done here by comparing
//! the open descriptor's inode with the path's `lstat`.
//!
//! - A directory or file this crate creates gets its exact mode (`0700` / `0600`), set through an open descriptor,
//!   since a umask can only remove bits. Only what this crate created is ever chmodded.
//! - Anything that already exists is the user's: if group or others can access it, it is [`BlobError::Insecure`]
//!   and left alone. A symlink at the final part of the path is [`BlobError::Symlink`] and never followed.
//!   Symlinks in the parents are fine (macOS's `/tmp` is one).
//! - Paths are normalised first, so a trailing `/` or `/.` can't get a symlink past the check.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::BlobError;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

pub(crate) fn io_err(path: &Path, e: io::Error) -> BlobError {
    BlobError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `path` without a trailing `/` or `/.`, which would make the OS resolve a symlink at its final part.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Refuses `path` if group or others can access it.
fn check_private(path: &Path, meta: &Metadata) -> Result<(), BlobError> {
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(BlobError::Insecure {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// `meta` is `path`'s `lstat`: refuses a symlink and anything but a directory.
fn check_dir(path: &Path, meta: &Metadata) -> Result<(), BlobError> {
    if meta.file_type().is_symlink() {
        return Err(BlobError::Symlink(path.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(BlobError::WrongType(path.to_path_buf()));
    }
    Ok(())
}

/// Like [`check_dir`], and refuses a directory group or others can access.
fn check_existing_dir(path: &Path, meta: &Metadata) -> Result<(), BlobError> {
    check_dir(path, meta)?;
    check_private(path, meta)
}

/// `meta` is `path`'s `lstat`: refuses a symlink and anything but a regular file.
pub(crate) fn check_regular_file(path: &Path, meta: &Metadata) -> Result<(), BlobError> {
    if meta.file_type().is_symlink() {
        return Err(BlobError::Symlink(path.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(BlobError::WrongType(path.to_path_buf()));
    }
    Ok(())
}

/// Creates `dir` exactly `0700`, or checks that an existing one is a real directory, not a symlink, that group and
/// others can't access. An existing directory is never chmodded. Missing parents are created `0700` minus the umask
/// and not chmodded, as `private_fs` does, since a parent may be shared with other things.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), BlobError> {
    let dir = &normalize(dir);
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(parent)
            .map_err(|e| io_err(parent, e))?;
    }
    match fs::DirBuilder::new().mode(DIR_MODE).create(dir) {
        Ok(()) => {
            // Ours: set the exact mode through a descriptor, after checking it is the directory we just made and
            // not something swapped in for it.
            let opened = File::open(dir).map_err(|e| io_err(dir, e))?;
            let fd_meta = opened.metadata().map_err(|e| io_err(dir, e))?;
            let path_meta = fs::symlink_metadata(dir).map_err(|e| io_err(dir, e))?;
            check_dir(dir, &path_meta)?;
            if (fd_meta.dev(), fd_meta.ino()) != (path_meta.dev(), path_meta.ino()) {
                return Err(BlobError::Symlink(dir.to_path_buf()));
            }
            opened
                .set_permissions(fs::Permissions::from_mode(DIR_MODE))
                .map_err(|e| io_err(dir, e))
        }
        // Already there (before us or racing us): the user's, so check it and never chmod it.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir).map_err(|e| io_err(dir, e))?;
            check_existing_dir(dir, &meta)
        }
        Err(e) => Err(io_err(dir, e)),
    }
}

/// Creates `path` exactly `0600` for writing; it must not exist. `O_EXCL` refuses a symlink there too.
pub(crate) fn create_new_private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
    Ok(file)
}

/// Opens an existing regular file for reading, refusing a symlink, anything but a regular file (checked before the
/// open, so a FIFO can't block it), a file swapped between the check and the open, and a file group or others can
/// access.
pub(crate) fn open_existing_private_file(path: &Path) -> Result<File, BlobError> {
    let before = fs::symlink_metadata(path).map_err(|e| io_err(path, e))?;
    check_regular_file(path, &before)?;
    let file = File::open(path).map_err(|e| io_err(path, e))?;
    let meta = file.metadata().map_err(|e| io_err(path, e))?;
    if (meta.dev(), meta.ino()) != (before.dev(), before.ino()) {
        return Err(BlobError::Symlink(path.to_path_buf()));
    }
    check_private(path, &meta)?;
    Ok(file)
}

/// Makes a rename or create inside `dir` durable.
pub(crate) fn sync_dir(dir: &Path) -> Result<(), BlobError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_err(dir, e))
}
