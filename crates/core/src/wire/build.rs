//! Which build of shepherd a process runs, exchanged in `_shepherd/hello` so a client can tell a host of another
//! build apart (spike T36, decision 0018).
//!
//! A build is identified by the sha256 digest of its executable. The `shepherd` crate reads the file and feeds its
//! bytes to a [`DigestHasher`]; digests are compared as bytes, never as text.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::base64;

/// The sha256 digest of an executable. Base64 on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct ExeDigest([u8; 32]);

impl ExeDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<ExeDigest> for String {
    fn from(d: ExeDigest) -> Self {
        base64::encode(&d.0)
    }
}

impl TryFrom<String> for ExeDigest {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        let bytes = base64::decode(&s).map_err(|e| e.to_string())?;
        let len = bytes.len();
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| format!("a digest is 32 bytes, not {len}"))
    }
}

/// Computes an [`ExeDigest`] from bytes fed to it in any chunks.
#[derive(Debug, Clone, Default)]
pub struct DigestHasher(Sha256);

impl DigestHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> ExeDigest {
        ExeDigest(self.0.finalize().into())
    }
}

/// The build a process runs, computed once at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    /// The crate version, for people to read only: builds are told apart by `digest`.
    pub version: String,
    /// The digest of the process's executable, as it was when the process started.
    pub digest: ExeDigest,
}
