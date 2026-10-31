//! An in-memory `MachineIdentity`: the stored key lives in memory shared by every clone, the way a key file is shared
//! by every host that opens the same root. New keys come from a process-wide counter, so every fresh fake is a
//! different machine and runs are deterministic. A key's id is its bytes in lowercase hex (the real adapter's is the
//! public key's): the same for the same key, and 64 hex digits like the real one.
//!
//! What a key file can be is set up front: [`FakeIdentity::holding`] (any bytes), [`FakeIdentity::holding_with_mode`]
//! (permissions as a file mode, checked the way the real adapter checks them), [`FakeIdentity::not_a_file`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{IdentityError, MachineIdentity, MachineKey};

/// What is stored where the key goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stored {
    Nothing,
    File { bytes: Vec<u8>, mode: u32 },
    NotAFile,
}

#[derive(Debug, Clone)]
pub struct FakeIdentity {
    stored: Arc<Mutex<Stored>>,
}

/// A fake machine key: 32 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeMachineKey([u8; 32]);

impl MachineKey for FakeMachineKey {
    fn machine_id(&self) -> MachineId {
        let hex: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
        MachineId::new(hex)
    }
}

/// Every fresh key is a different one.
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

const PRIVATE: u32 = 0o600;

impl Default for FakeIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeIdentity {
    /// Nothing stored yet: the first load makes a key.
    pub fn new() -> Self {
        Self::with(Stored::Nothing)
    }

    /// `bytes` stored where the key goes, readable by the user alone.
    pub fn holding(bytes: &[u8]) -> Self {
        Self::holding_with_mode(bytes, PRIVATE)
    }

    /// `bytes` stored where the key goes, with file mode `mode`.
    pub fn holding_with_mode(bytes: &[u8], mode: u32) -> Self {
        Self::with(Stored::File {
            bytes: bytes.to_vec(),
            mode,
        })
    }

    /// Something other than a file where the key goes.
    pub fn not_a_file() -> Self {
        Self::with(Stored::NotAFile)
    }

    fn with(stored: Stored) -> Self {
        Self {
            stored: Arc::new(Mutex::new(stored)),
        }
    }

    /// The stored key file's bytes, if a file is stored.
    pub fn stored(&self) -> Option<Vec<u8>> {
        match &*self.stored.lock().unwrap_or_else(PoisonError::into_inner) {
            Stored::File { bytes, .. } => Some(bytes.clone()),
            Stored::Nothing | Stored::NotAFile => None,
        }
    }
}

impl MachineIdentity for FakeIdentity {
    type Key = FakeMachineKey;

    fn load_or_create(&self) -> Result<FakeMachineKey, IdentityError> {
        let mut stored = self.stored.lock().unwrap_or_else(PoisonError::into_inner);
        match &*stored {
            Stored::Nothing => {
                let mut key = [0xa5; 32];
                let n = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
                key[..8].copy_from_slice(&n.to_le_bytes());
                *stored = Stored::File {
                    bytes: key.to_vec(),
                    mode: PRIVATE,
                };
                Ok(FakeMachineKey(key))
            }
            Stored::NotAFile => Err(IdentityError::WrongType),
            // As for a file: one the user can't read fails to open before its mode is looked at.
            Stored::File { mode, .. } if mode & 0o400 == 0 => {
                Err(IdentityError::Io("permission denied".into()))
            }
            Stored::File { mode, .. } if mode & 0o077 != 0 => {
                Err(IdentityError::Insecure { mode: *mode })
            }
            Stored::File { bytes, .. } => match <[u8; 32]>::try_from(bytes.as_slice()) {
                Ok(key) => Ok(FakeMachineKey(key)),
                Err(_) => Err(IdentityError::WrongSize {
                    len: bytes.len() as u64,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::identity::IdentityFixture;

    struct FakeFixture;

    impl IdentityFixture for FakeFixture {
        type Identity = FakeIdentity;

        fn empty(&mut self) -> FakeIdentity {
            FakeIdentity::new()
        }

        fn reopen(&mut self, identity: &FakeIdentity) -> FakeIdentity {
            identity.clone()
        }

        fn holding(&mut self, bytes: &[u8]) -> FakeIdentity {
            FakeIdentity::holding(bytes)
        }

        fn holding_with_mode(&mut self, bytes: &[u8], mode: u32) -> FakeIdentity {
            FakeIdentity::holding_with_mode(bytes, mode)
        }

        fn not_a_file(&mut self) -> FakeIdentity {
            FakeIdentity::not_a_file()
        }

        fn stored(&self, identity: &FakeIdentity) -> Option<Vec<u8>> {
            identity.stored()
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::identity_contract!(FakeFixture);
    }
}
