//! Private directories and files under `~/.shepherd` (decision 0010): every adapter that creates something there
//! goes through these helpers, so they all follow the same rules, whatever the umask.
//!
//! - A directory or file this call creates gets its exact mode (`0700` / `0600`), set through a descriptor opened
//!   with `O_NOFOLLOW`, since a umask can only remove bits. Only what this call created is ever chmodded.
//! - Anything that already exists is the user's: if group or others can access it, it is
//!   [`PrivateFsError::Insecure`] and left alone. A symlink at the final part of the path is
//!   [`PrivateFsError::Symlink`] and is never followed, for a directory, a file being created or an existing file
//!   being opened ([`open_existing_private_file`]). Symlinks in the parents are fine (macOS's `/tmp` is one).
//! - An existing file must be a regular file, or it is [`PrivateFsError::WrongType`]. It is opened non-blocking
//!   and checked through the descriptor, so a FIFO planted where a file goes can't hang the open.
//! - Paths are normalised first, so a trailing `/` (which makes the OS resolve `link/` through the link) or a
//!   trailing `/.` can't get a symlink past the check.
//! - There is no check-then-create gap: the final part is created with a plain `mkdir` (or `O_CREAT|O_EXCL`), and
//!   `AlreadyExists` means someone else got there first, so the existing entry is checked instead.
//!
//! Missing parents are created `0700` minus the umask and are not chmodded, since a parent may be shared with
//! other things. Under a umask that strips owner bits (e.g. `277`) a created parent comes out `0500`, and creating
//! the next part inside it then fails with "permission denied". The final part is always exact.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivateFsError {
    /// The path exists and group or others have some access to it. It is never chmodded: the user fixes it.
    #[error("{path} has mode {mode:o}; it must not be accessible to group or others")]
    Insecure { path: PathBuf, mode: u32 },
    /// The final part of the path is a symlink. It is never followed.
    #[error("{0} is a symlink; refusing to follow it")]
    Symlink(PathBuf),
    /// A directory was wanted and something else is there, or the other way round.
    #[error("{0} exists and is the wrong kind of file")]
    WrongType(PathBuf),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

fn io_err(path: &Path, e: io::Error) -> PrivateFsError {
    PrivateFsError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Refuses `path` if group or others can access it. `meta` is `path`'s metadata, from an open descriptor or
/// `symlink_metadata`.
fn check_private(path: &Path, meta: &Metadata) -> Result<(), PrivateFsError> {
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PrivateFsError::Insecure {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// `path` without a trailing `/` or `/.`, which would make the OS resolve a symlink at its final part.
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Refuses `path` if it is a symlink. `file_type` must not follow links (`symlink_metadata`, `DirEntry::file_type`).
pub fn refuse_symlink(path: &Path, file_type: fs::FileType) -> Result<(), PrivateFsError> {
    if file_type.is_symlink() {
        return Err(PrivateFsError::Symlink(path.to_path_buf()));
    }
    Ok(())
}

/// Creates `dir` exactly `0700` (and missing parents, see the module docs), or checks that an existing one is a
/// real directory, not a symlink, that group and others can't access.
pub fn ensure_private_dir(dir: &Path) -> Result<(), PrivateFsError> {
    let dir = &normalize(dir);
    match fs::symlink_metadata(dir) {
        Ok(meta) => return check_existing_dir(dir, &meta),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(dir, e)),
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(parent)
            .map_err(|e| io_err(parent, e))?;
    }
    create_final_dir(dir)
}

/// Creates the final part of `dir` exactly `0700`, or, if something is already there, checks it like any
/// existing dir and never chmods it.
fn create_final_dir(dir: &Path) -> Result<(), PrivateFsError> {
    match fs::DirBuilder::new().mode(DIR_MODE).create(dir) {
        Ok(()) => {
            // Ours: set the exact mode through a descriptor that can't have been swapped for a symlink.
            let created = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(dir)
                .map_err(|e| open_err(dir, e))?;
            created
                .set_permissions(fs::Permissions::from_mode(DIR_MODE))
                .map_err(|e| io_err(dir, e))
        }
        // Someone else created it between our check and our mkdir: theirs, so check it, never chmod it.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir).map_err(|e| io_err(dir, e))?;
            check_existing_dir(dir, &meta)
        }
        Err(e) => Err(io_err(dir, e)),
    }
}

fn check_existing_dir(dir: &Path, meta: &Metadata) -> Result<(), PrivateFsError> {
    refuse_symlink(dir, meta.file_type())?;
    if !meta.is_dir() {
        return Err(PrivateFsError::WrongType(dir.to_path_buf()));
    }
    check_private(dir, meta)
}

/// Opens `path` with `opts`, creating it exactly `0600` if missing, or checking that an existing one is a regular
/// file, not a symlink, that group and others can't access. The parent directory must already exist.
pub fn create_or_open_private_file(
    path: &Path,
    opts: &OpenOptions,
) -> Result<File, PrivateFsError> {
    let path = &normalize(path);
    let mut new = opts.clone();
    new.create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    match new.open(path) {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
                .map_err(|e| io_err(path, e))?;
            Ok(file)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => open_existing_private_file(path, opts),
        Err(e) => Err(open_err(path, e)),
    }
}

/// Opens an existing `path` with `opts` (which must not create it), refusing a symlink at its final part, and
/// checking through the open descriptor that it is a regular file that group and others can't access.
///
/// It opens with `O_NONBLOCK`, so a FIFO planted at `path` can't block the open, and clears it again
/// once the descriptor is known to be a regular file.
pub fn open_existing_private_file(path: &Path, opts: &OpenOptions) -> Result<File, PrivateFsError> {
    let path = &normalize(path);
    let file = opts
        .clone()
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| open_err(path, e))?;
    let meta = file.metadata().map_err(|e| io_err(path, e))?;
    if !meta.is_file() {
        return Err(PrivateFsError::WrongType(path.to_path_buf()));
    }
    check_private(path, &meta)?;
    clear_nonblock(&file).map_err(|e| io_err(path, e))?;
    Ok(file)
}

fn clear_nonblock(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: F_GETFL and F_SETFL on a descriptor `file` owns for the duration of the calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `O_NOFOLLOW` reports a symlink as `ELOOP`. With `O_NONBLOCK`, opening a FIFO for writing with no reader is
/// `ENXIO`: not a regular file.
fn open_err(path: &Path, e: io::Error) -> PrivateFsError {
    match e.raw_os_error() {
        Some(libc::ELOOP) => PrivateFsError::Symlink(path.to_path_buf()),
        Some(libc::ENXIO) => PrivateFsError::WrongType(path.to_path_buf()),
        _ => io_err(path, e),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        PrivateFsError, create_final_dir, create_or_open_private_file, ensure_private_dir,
        open_existing_private_file,
    };

    fn private_tempdir() -> TempDir {
        let dir = TempDir::new().unwrap();
        chmod(dir.path(), 0o700);
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_opts() -> OpenOptions {
        let mut opts = OpenOptions::new();
        opts.write(true);
        opts
    }

    /// Run directly under the caller's umask, and again in child processes under extreme umasks by
    /// `modes_are_exact_whatever_the_umask`.
    #[test]
    fn creates_the_dir_0700_and_the_file_0600() {
        let dir = private_tempdir();
        let created = dir.path().join("shepherd");
        ensure_private_dir(&created).unwrap();
        let file = created.join("f");
        create_or_open_private_file(&file, &write_opts()).unwrap();

        assert_eq!(mode(&created), 0o700);
        assert_eq!(mode(&file), 0o600);
    }

    /// The process umask is never changed (it's process-wide), so this reruns the test above in a child under a
    /// permissive umask (000) and one that strips owner bits (277).
    #[test]
    fn modes_are_exact_whatever_the_umask() {
        let exe = std::env::current_exe().unwrap();
        for umask in ["000", "277"] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "umask {umask} && exec \"$0\" --exact --test-threads=1 \"$1\""
                ))
                .arg(&exe)
                .arg("private_fs::tests::creates_the_dir_0700_and_the_file_0600")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "umask {umask}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn missing_parents_are_created() {
        let dir = private_tempdir();
        let deep = dir.path().join("a/b/c");
        ensure_private_dir(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        // 0700 minus the umask, and test umasks don't strip owner bits.
        assert_eq!(mode(&dir.path().join("a")), 0o700);
        assert_eq!(mode(&dir.path().join("a/b")), 0o700);
    }

    /// Set by `parents_created_under_umask_277_are_0500` for the child it runs under that umask.
    const UMASK_277: &str = "SHEPHERD_TEST_UMASK_277";

    /// Only meaningful in the child of the test below; passes trivially anywhere else.
    #[test]
    fn under_umask_277_a_created_parent_is_0500_and_blocks_the_next_part() {
        if std::env::var_os(UMASK_277).is_none() {
            return;
        }
        let dir = private_tempdir();
        let result = ensure_private_dir(&dir.path().join("a/b"));
        assert!(
            matches!(&result, Err(PrivateFsError::Io { message, .. }) if message.contains("ermission denied")),
            "{result:?}"
        );
        assert_eq!(mode(&dir.path().join("a")), 0o500);
        chmod(&dir.path().join("a"), 0o700);
    }

    /// Documents the module-doc note: parents are not chmodded, so under umask 277 they come out 0500.
    #[test]
    fn parents_created_under_umask_277_are_0500() {
        let exe = std::env::current_exe().unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg("umask 277 && exec \"$0\" --exact --test-threads=1 \"$1\"")
            .arg(&exe)
            .arg("private_fs::tests::under_umask_277_a_created_parent_is_0500_and_blocks_the_next_part")
            .env(UMASK_277, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `link/` and `link/.` resolve through the link for both `lstat` and `O_NOFOLLOW`, so they are normalised away.
    #[test]
    fn a_trailing_slash_or_dot_does_not_get_a_symlink_past_the_check() {
        for suffix in ["/", "/.", "//"] {
            let (dir, outside) = (private_tempdir(), private_tempdir());
            let link = dir.path().join("d");
            symlink(outside.path(), &link).unwrap();
            let mut given = link.clone().into_os_string();
            given.push(suffix);

            assert_eq!(
                ensure_private_dir(Path::new(&given)),
                Err(PrivateFsError::Symlink(link.clone())),
                "{suffix}"
            );
        }
    }

    #[test]
    fn an_existing_private_dir_is_accepted_as_is() {
        let dir = private_tempdir();
        let existing = dir.path().join("d");
        fs::create_dir(&existing).unwrap();
        chmod(&existing, 0o500);

        ensure_private_dir(&existing).unwrap();
        assert_eq!(mode(&existing), 0o500, "an existing dir is never chmodded");
    }

    #[test]
    fn an_existing_dir_open_to_group_or_others_is_insecure_and_left_alone() {
        for open_mode in [0o755, 0o770, 0o701] {
            let dir = private_tempdir();
            let existing = dir.path().join("d");
            fs::create_dir(&existing).unwrap();
            chmod(&existing, open_mode);

            assert_eq!(
                ensure_private_dir(&existing),
                Err(PrivateFsError::Insecure {
                    path: existing.clone(),
                    mode: open_mode
                })
            );
            assert_eq!(mode(&existing), open_mode);
        }
    }

    /// Even a symlink to a private dir: following it would put our files wherever whoever planted it chose.
    #[test]
    fn a_symlink_at_the_final_part_is_refused_not_followed_or_chmodded() {
        for target_mode in [0o700, 0o755] {
            let dir = private_tempdir();
            let target = dir.path().join("elsewhere");
            fs::create_dir(&target).unwrap();
            chmod(&target, target_mode);
            let link = dir.path().join("d");
            symlink(&target, &link).unwrap();

            assert_eq!(
                ensure_private_dir(&link),
                Err(PrivateFsError::Symlink(link.clone()))
            );
            assert_eq!(mode(&target), target_mode);
            assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        }
    }

    #[test]
    fn a_dangling_symlink_at_the_final_part_is_refused_and_nothing_is_created() {
        let dir = private_tempdir();
        let target = dir.path().join("nowhere");
        let link = dir.path().join("d");
        symlink(&target, &link).unwrap();

        assert_eq!(
            ensure_private_dir(&link),
            Err(PrivateFsError::Symlink(link.clone()))
        );
        assert!(!target.exists());
    }

    #[test]
    fn symlinks_in_the_parents_are_followed() {
        let dir = private_tempdir();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        symlink(&real, dir.path().join("alias")).unwrap();

        ensure_private_dir(&dir.path().join("alias/d")).unwrap();
        assert_eq!(mode(&real.join("d")), 0o700);
    }

    #[test]
    fn a_file_where_the_dir_goes_is_wrong_type() {
        let dir = private_tempdir();
        let path = dir.path().join("d");
        fs::write(&path, b"").unwrap();
        assert_eq!(
            ensure_private_dir(&path),
            Err(PrivateFsError::WrongType(path.clone()))
        );
    }

    /// Racing creators all succeed: whoever loses the `mkdir` race sees `AlreadyExists` and checks the winner's dir.
    #[test]
    fn concurrent_creators_all_succeed() {
        let dir = private_tempdir();
        let target = dir.path().join("a/b/d");
        let barrier = Arc::new(Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (target, barrier) = (target.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    ensure_private_dir(&target)
                })
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), Ok(()));
        }
        assert_eq!(mode(&target), 0o700);
    }

    #[test]
    fn an_existing_private_file_is_opened_as_is() {
        let dir = private_tempdir();
        let path = dir.path().join("f");
        fs::write(&path, b"keep").unwrap();
        chmod(&path, 0o600);

        create_or_open_private_file(&path, &write_opts()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn an_existing_file_open_to_group_or_others_is_insecure_and_left_alone() {
        let dir = private_tempdir();
        let path = dir.path().join("f");
        fs::write(&path, b"").unwrap();
        chmod(&path, 0o644);

        assert_eq!(
            create_or_open_private_file(&path, &write_opts()).map(|_| ()),
            Err(PrivateFsError::Insecure {
                path: path.clone(),
                mode: 0o644
            })
        );
        assert_eq!(mode(&path), 0o644);
    }

    #[test]
    fn open_existing_refuses_a_symlink_and_leaves_its_target_untouched() {
        let dir = private_tempdir();
        let target = dir.path().join("target");
        fs::write(&target, b"ghp_secrettoken").unwrap();
        chmod(&target, 0o600);
        let link = dir.path().join("f");
        symlink(&target, &link).unwrap();

        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        assert_eq!(
            open_existing_private_file(&link, &opts).map(|_| ()),
            Err(PrivateFsError::Symlink(link.clone()))
        );
        assert_eq!(fs::read(&target).unwrap(), b"ghp_secrettoken");
    }

    #[test]
    fn open_existing_checks_kind_and_mode_and_never_creates() {
        let dir = private_tempdir();
        let mut opts = OpenOptions::new();
        opts.read(true);

        let missing = dir.path().join("missing");
        assert!(matches!(
            open_existing_private_file(&missing, &opts),
            Err(PrivateFsError::Io { .. })
        ));
        assert!(!missing.exists());

        let open = dir.path().join("open");
        fs::write(&open, b"").unwrap();
        chmod(&open, 0o640);
        assert_eq!(
            open_existing_private_file(&open, &opts).map(|_| ()),
            Err(PrivateFsError::Insecure {
                path: open.clone(),
                mode: 0o640
            })
        );

        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        chmod(&sub, 0o700);
        assert_eq!(
            open_existing_private_file(&sub, &opts).map(|_| ()),
            Err(PrivateFsError::WrongType(sub.clone()))
        );

        let private = dir.path().join("private");
        fs::write(&private, b"ok").unwrap();
        chmod(&private, 0o600);
        assert!(open_existing_private_file(&private, &opts).is_ok());
    }

    #[test]
    fn a_symlink_where_the_file_goes_is_refused_and_its_target_untouched() {
        let dir = private_tempdir();
        let target = dir.path().join("target");
        fs::write(&target, b"precious").unwrap();
        chmod(&target, 0o600);
        let link = dir.path().join("f");
        symlink(&target, &link).unwrap();

        let mut opts = OpenOptions::new();
        opts.append(true);
        match create_or_open_private_file(&link, &opts) {
            Err(e) => assert_eq!(e, PrivateFsError::Symlink(link.clone())),
            Ok(mut f) => {
                let _ = f.write_all(b"oops");
                panic!("followed the symlink");
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"precious");
    }

    /// Makes a FIFO at `path`, chmodded 0600 so only its kind can make it unacceptable.
    pub(crate) fn private_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        chmod(path, 0o600);
    }

    /// Runs `f` on another thread and returns its result, failing (not hanging) if it doesn't finish within a
    /// generous liveness bound. This is not a timing assertion: a blocked `open` would never finish at all.
    pub(crate) fn within_liveness_bound<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("blocked instead of returning: {e}"))
    }

    /// Opening a FIFO blocks until the other end opens, so a FIFO planted where a private file goes must be
    /// refused without ever waiting on it, for reading and for appending.
    #[test]
    fn a_fifo_is_wrong_type_and_opening_it_never_blocks() {
        let dir = private_tempdir();
        let fifo = dir.path().join("f");
        private_fifo(&fifo);

        let mut read = OpenOptions::new();
        read.read(true);
        let mut append = OpenOptions::new();
        append.append(true);
        for opts in [read, append] {
            let path = fifo.clone();
            let opened =
                within_liveness_bound(move || open_existing_private_file(&path, &opts).map(|_| ()));
            assert_eq!(opened, Err(PrivateFsError::WrongType(fifo.clone())));
        }
    }

    /// A reader already on the FIFO makes the non-blocking open succeed; the fstat check still refuses it.
    #[test]
    fn a_fifo_with_a_reader_is_still_wrong_type() {
        let dir = private_tempdir();
        let fifo = dir.path().join("f");
        private_fifo(&fifo);
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();

        let path = fifo.clone();
        let opened = within_liveness_bound(move || {
            create_or_open_private_file(&path, OpenOptions::new().append(true)).map(|_| ())
        });
        assert_eq!(opened, Err(PrivateFsError::WrongType(fifo)));
    }

    /// The file that is returned is blocking again: `O_NONBLOCK` is only for the open.
    #[test]
    fn an_opened_private_file_is_not_left_non_blocking() {
        use std::os::fd::AsRawFd;
        let dir = private_tempdir();
        let path = dir.path().join("f");
        fs::write(&path, b"").unwrap();
        chmod(&path, 0o600);

        let file = open_existing_private_file(&path, OpenOptions::new().read(true)).unwrap();
        // SAFETY: F_GETFL on a descriptor we own.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    /// The `mkdir` lost a race: what's there is someone else's, so it is checked and never chmodded.
    #[test]
    fn losing_the_mkdir_race_to_an_open_dir_is_insecure_and_leaves_it_alone() {
        let dir = private_tempdir();
        let theirs = dir.path().join("d");
        fs::create_dir(&theirs).unwrap();
        chmod(&theirs, 0o755);

        assert_eq!(
            create_final_dir(&theirs),
            Err(PrivateFsError::Insecure {
                path: theirs.clone(),
                mode: 0o755
            })
        );
        assert_eq!(mode(&theirs), 0o755);
    }

    #[test]
    fn losing_the_mkdir_race_to_a_symlink_refuses_it_and_leaves_its_target_alone() {
        let dir = private_tempdir();
        let target = dir.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        chmod(&target, 0o755);
        let link = dir.path().join("d");
        symlink(&target, &link).unwrap();

        assert_eq!(
            create_final_dir(&link),
            Err(PrivateFsError::Symlink(link.clone()))
        );
        assert_eq!(mode(&target), 0o755);
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }
}
