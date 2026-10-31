//! The machine's identity (T29, decision 0019): a secret key the host generates on its first run and keeps, so the
//! machine is the same machine across restarts and updates in place. Its [`MachineId`] stamps every entry the host
//! appends.
//!
//! Core never sees the secret. The key is the adapter's own type ([`MachineIdentity::Key`]), and core only asks it for
//! the id derived from it ([`MachineKey::machine_id`]). The real adapter keeps an iroh secret key in
//! `~/.shepherd/identity.key`, and its id is the iroh node id's lowercase hex.
//!
//! What is stored is never overwritten: a stored key that can't be used is an error for the user to fix, not a reason
//! to make a new one (which would make this a different machine to every peer).

use crate::ids::MachineId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The stored key can be read by group or others (mode as stored). It is never chmodded.
    #[error("the machine key has mode {mode:o}; it must be readable by you alone (0600)")]
    Insecure { mode: u32 },
    /// The stored key isn't a key: it holds `len` bytes.
    #[error("the machine key holds {len} bytes, not a 32-byte key")]
    WrongSize { len: u64 },
    /// Something other than a regular file is where the key goes (a directory, a symlink, a FIFO, ...).
    #[error("the machine key is not a regular file")]
    WrongType,
    /// It couldn't be read or created.
    #[error("{0}")]
    Io(String),
}

/// A loaded machine key. Only the id derived from it reaches core.
pub trait MachineKey {
    /// The machine's id: the same for the same key, different for different keys.
    fn machine_id(&self) -> MachineId;
}

pub trait MachineIdentity {
    type Key: MachineKey;

    /// The stored key, or a new one, generated and stored, when there is none. The same stored key is returned on
    /// every call, from this value or any other over the same storage. A stored key that can't be used is an error
    /// and is left as it is.
    fn load_or_create(&self) -> Result<Self::Key, IdentityError>;
}
