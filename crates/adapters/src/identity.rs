//! `MachineIdentity` on a key file: `<root>/identity.key` (`~/.shepherd/identity.key`), the 32 raw bytes of an iroh
//! (ed25519) secret key (decision 0019). The machine id is the key's iroh node id, in iroh's lowercase hex.
//!
//! The file goes through the shared [`private_fs`] helpers (decision 0010): the root is made `0700` (or checked), and
//! the key is created `0600` with `O_CREAT|O_EXCL`, never over anything already there. An existing key file is opened
//! without following a symlink and checked through its descriptor: one group or others can access, one that isn't a
//! regular file, or one that doesn't hold exactly 32 bytes is an error, and is left as it is. A new key that can't be
//! written in full (a full disk) is removed again; a host killed while writing one leaves a short file, which is then
//! reported the same way rather than replaced.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use iroh::SecretKey;
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{IdentityError, MachineIdentity, MachineKey};

use crate::private_fs::{self, PrivateFsError};

/// The key file, in the root.
pub const IDENTITY_FILE: &str = "identity.key";

const KEY_LEN: u64 = 32;

pub struct KeyFileIdentity {
    root: PathBuf,
}

impl KeyFileIdentity {
    /// The identity kept in `<root>/identity.key`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where the key is kept.
    pub fn path(&self) -> PathBuf {
        self.root.join(IDENTITY_FILE)
    }
}

/// The machine's iroh secret key.
pub struct IrohMachineKey(SecretKey);

impl IrohMachineKey {
    /// The key itself, for the iroh endpoint to bind with.
    pub fn secret(&self) -> &SecretKey {
        &self.0
    }
}

impl MachineKey for IrohMachineKey {
    fn machine_id(&self) -> MachineId {
        MachineId::new(self.0.public().to_string())
    }
}

impl MachineIdentity for KeyFileIdentity {
    type Key = IrohMachineKey;

    fn load_or_create(&self) -> Result<IrohMachineKey, IdentityError> {
        // The root's own problems are about the root, not the key: reported as they are.
        private_fs::ensure_private_dir(&self.root).map_err(|e| IdentityError::Io(e.to_string()))?;
        let path = self.path();
        let mut write = OpenOptions::new();
        write.write(true);
        match private_fs::create_new_private_file(&path, &write).map_err(key_error)? {
            Some(file) => create(&path, file),
            None => load(&path),
        }
    }
}

/// Generates a key into `file`, which this call just created.
fn create(path: &Path, mut file: std::fs::File) -> Result<IrohMachineKey, IdentityError> {
    let key = SecretKey::generate();
    // Once, on the first run: this file is the machine, so it must survive a power cut too.
    let written = file
        .write_all(&key.to_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        // This call created the file, so it removes it: a short one left behind (the disk was full) would be refused
        // as the wrong size on every later start, while a missing one is simply made again.
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(IdentityError::Io(format!("{}: {e}", path.display())));
    }
    Ok(IrohMachineKey(key))
}

/// Reads the existing key file at `path`.
fn load(path: &Path) -> Result<IrohMachineKey, IdentityError> {
    let mut read = OpenOptions::new();
    read.read(true);
    let file = private_fs::open_existing_private_file(path, &read).map_err(key_error)?;
    let io = |e: std::io::Error| IdentityError::Io(format!("{}: {e}", path.display()));
    let len = file.metadata().map_err(io)?.len();
    if len != KEY_LEN {
        return Err(IdentityError::WrongSize { len });
    }
    // One byte more than a key, so a file that grew since its length was read is caught too.
    let mut bytes = Vec::with_capacity(33);
    file.take(KEY_LEN + 1).read_to_end(&mut bytes).map_err(io)?;
    let key = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| IdentityError::WrongSize {
        len: bytes.len() as u64,
    })?;
    Ok(IrohMachineKey(SecretKey::from_bytes(&key)))
}

fn key_error(e: PrivateFsError) -> IdentityError {
    match e {
        PrivateFsError::Insecure { mode, .. } => IdentityError::Insecure { mode },
        PrivateFsError::Symlink(_) | PrivateFsError::WrongType(_) => IdentityError::WrongType,
        e @ PrivateFsError::Io { .. } => IdentityError::Io(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use shepherd_testkit::contracts::identity::IdentityFixture;
    use tempfile::TempDir;

    use super::*;

    /// Each identity gets a root of its own in one temp dir, which lives as long as the fixture (one test).
    struct KeyFileFixture {
        dir: TempDir,
        next: u32,
    }

    impl KeyFileFixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir, next: 0 }
        }

        /// A fresh root that doesn't exist yet: the adapter makes it.
        fn fresh_root(&mut self) -> PathBuf {
            self.next += 1;
            self.dir.path().join(format!("root-{}", self.next))
        }

        /// A fresh root, made `0700`, with `bytes` in its key file at `mode`.
        fn root_holding(&mut self, bytes: &[u8], mode: u32) -> PathBuf {
            let root = self.fresh_root();
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let key = root.join(IDENTITY_FILE);
            fs::write(&key, bytes).unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(mode)).unwrap();
            root
        }
    }

    impl IdentityFixture for KeyFileFixture {
        type Identity = KeyFileIdentity;

        fn empty(&mut self) -> KeyFileIdentity {
            KeyFileIdentity::new(self.fresh_root())
        }

        fn reopen(&mut self, identity: &KeyFileIdentity) -> KeyFileIdentity {
            KeyFileIdentity::new(identity.root.clone())
        }

        fn holding(&mut self, bytes: &[u8]) -> KeyFileIdentity {
            KeyFileIdentity::new(self.root_holding(bytes, 0o600))
        }

        fn holding_with_mode(&mut self, bytes: &[u8], mode: u32) -> KeyFileIdentity {
            KeyFileIdentity::new(self.root_holding(bytes, mode))
        }

        /// A directory where the key file goes.
        fn not_a_file(&mut self) -> KeyFileIdentity {
            let root = self.fresh_root();
            fs::create_dir_all(root.join(IDENTITY_FILE)).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            KeyFileIdentity::new(root)
        }

        fn stored(&self, identity: &KeyFileIdentity) -> Option<Vec<u8>> {
            let path = identity.path();
            let meta = fs::symlink_metadata(&path).ok()?;
            if !meta.is_file() {
                return None;
            }
            fs::read(&path).ok()
        }
    }

    shepherd_testkit::identity_contract!(KeyFileFixture::new());

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_key_file_is_created_0600_in_a_0700_root() {
        let mut f = KeyFileFixture::new();
        let identity = f.empty();
        identity.load_or_create().unwrap();
        assert_eq!(mode(&identity.root), 0o700);
        assert_eq!(mode(&identity.path()), 0o600);
    }

    #[test]
    fn the_id_is_the_iroh_node_id_of_the_stored_key() {
        let mut f = KeyFileFixture::new();
        let identity = f.empty();
        let id = identity.load_or_create().unwrap().machine_id();
        let bytes: [u8; 32] = fs::read(identity.path()).unwrap().try_into().unwrap();
        let node = SecretKey::from_bytes(&bytes).public();
        assert_eq!(id.as_str(), node.to_string());
    }

    #[test]
    fn the_loaded_key_is_the_stored_secret() {
        let mut f = KeyFileFixture::new();
        let identity = f.holding(&[5; 32]);
        let key = identity.load_or_create().unwrap();
        assert_eq!(key.secret().to_bytes(), [5; 32]);
    }

    #[test]
    fn a_symlinked_key_file_is_the_wrong_type_and_never_followed() {
        let mut f = KeyFileFixture::new();
        let target = f.root_holding(&[6; 32], 0o600).join(IDENTITY_FILE);
        let identity = f.empty();
        fs::create_dir(&identity.root).unwrap();
        fs::set_permissions(&identity.root, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, identity.path()).unwrap();
        let result = identity.load_or_create().map(|k| k.machine_id());
        assert_eq!(result, Err(IdentityError::WrongType));
        assert!(fs::symlink_metadata(identity.path()).unwrap().is_symlink());
    }

    /// Set by `a_key_that_cannot_be_written_leaves_no_file` for the child it runs with no room to write files.
    const NO_ROOM: &str = "SHEPHERD_TEST_NO_ROOM_FOR_FILES";

    /// Only meaningful in the child of the test below; passes trivially anywhere else.
    #[test]
    fn with_no_room_to_write_a_new_key_is_an_io_error_and_no_file_is_left() {
        if std::env::var_os(NO_ROOM).is_none() {
            return;
        }
        let mut f = KeyFileFixture::new();
        let identity = f.empty();
        for _ in 0..2 {
            let result = identity.load_or_create().map(|k| k.machine_id());
            // Not `WrongSize`: a short file left by the first try would be refused that way forever.
            assert!(matches!(result, Err(IdentityError::Io(_))), "{result:?}");
            assert!(!identity.path().exists(), "a short key file was left");
        }
    }

    /// The disk filling up while a new key is written (ENOSPC), stood in for by a file size limit of 0 (`ulimit -f 0`,
    /// with SIGXFSZ ignored so the write fails with EFBIG instead of killing the process), in a child process: the
    /// limit is process-wide.
    #[test]
    fn a_key_that_cannot_be_written_leaves_no_file() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' XFSZ && ulimit -f 0 && exec \"$0\" --exact --test-threads=1 \"$1\"")
            .arg(&exe)
            .arg("identity::tests::with_no_room_to_write_a_new_key_is_an_io_error_and_no_file_is_left")
            .env(NO_ROOM, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_root_others_can_access_is_refused_and_left_alone() {
        let mut f = KeyFileFixture::new();
        let identity = f.empty();
        fs::create_dir(&identity.root).unwrap();
        fs::set_permissions(&identity.root, fs::Permissions::from_mode(0o755)).unwrap();
        let result = identity.load_or_create().map(|k| k.machine_id());
        assert!(matches!(result, Err(IdentityError::Io(_))), "{result:?}");
        assert_eq!(mode(&identity.root), 0o755);
        assert!(!identity.path().exists());
    }
}
