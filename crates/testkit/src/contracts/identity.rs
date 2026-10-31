//! The `MachineIdentity` contract. Identities are built through an [`IdentityFixture`], over storage in each state the
//! contract needs: empty, holding some bytes, holding a key others can read, or holding something that isn't a file.
//! For the key-file adapter the storage is a directory standing in for `~/.shepherd`; for the fake it is shared memory.
//!
//! Run it with [`identity_contract!`](crate::identity_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Expands to one `#[test]` per `MachineIdentity` contract case. `$fixture` is an expression building a fresh
/// [`IdentityFixture`] for each test, e.g. `shepherd_testkit::identity_contract!(KeyFileFixture::new());`.
///
/// New cases go in the list below.
#[macro_export]
macro_rules! identity_contract {
    ($fixture:expr) => {
        $crate::identity_contract!(@cases $fixture;
            first_load_stores_a_32_byte_key,
            loading_again_gives_the_same_id,
            a_reopened_identity_gives_the_same_id,
            separate_storages_get_different_ids,
            the_id_is_64_lowercase_hex_digits,
            a_stored_key_is_loaded_and_kept,
            the_id_depends_only_on_the_key,
            a_stored_key_of_the_wrong_size_is_an_error_and_kept,
            a_key_others_can_read_is_insecure_and_kept,
            a_key_that_cannot_be_read_is_an_io_error,
            something_other_than_a_file_is_the_wrong_type,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::identity::$case(&mut $fixture);
            }
        )*
    };
}

use shepherd_core::ids::MachineId;
use shepherd_core::ports::{IdentityError, MachineIdentity, MachineKey};

/// Builds identities over storage in each state the contract needs. Each call but [`IdentityFixture::reopen`] gets
/// storage of its own.
pub trait IdentityFixture {
    type Identity: MachineIdentity;
    /// Storage holding nothing yet: a first run.
    fn empty(&mut self) -> Self::Identity;
    /// Another identity over the same storage as `identity`, as a restarted host opens it.
    fn reopen(&mut self, identity: &Self::Identity) -> Self::Identity;
    /// Storage holding `bytes` where the key goes, readable by the user alone.
    fn holding(&mut self, bytes: &[u8]) -> Self::Identity;
    /// Storage holding a 32-byte key with mode `mode` (for the file adapter, its file mode).
    fn holding_with_mode(&mut self, bytes: &[u8], mode: u32) -> Self::Identity;
    /// Storage with something other than a regular file where the key goes (for the file adapter, a directory).
    fn not_a_file(&mut self) -> Self::Identity;
    /// What is stored where the key goes, if it is a file the test can read.
    fn stored(&self, identity: &Self::Identity) -> Option<Vec<u8>>;
}

fn id<I: MachineIdentity>(identity: &I) -> MachineId {
    identity.load_or_create().unwrap().machine_id()
}

pub fn first_load_stores_a_32_byte_key<F: IdentityFixture>(f: &mut F) {
    let identity = f.empty();
    assert_eq!(f.stored(&identity), None, "empty storage holds nothing");
    identity.load_or_create().unwrap();
    let stored = f.stored(&identity).expect("a key is stored");
    assert_eq!(stored.len(), 32);
}

pub fn loading_again_gives_the_same_id<F: IdentityFixture>(f: &mut F) {
    let identity = f.empty();
    let first = id(&identity);
    let stored = f.stored(&identity);
    assert_eq!(id(&identity), first);
    assert_eq!(f.stored(&identity), stored, "the key is not replaced");
}

pub fn a_reopened_identity_gives_the_same_id<F: IdentityFixture>(f: &mut F) {
    let identity = f.empty();
    let first = id(&identity);
    let reopened = f.reopen(&identity);
    assert_eq!(id(&reopened), first);
}

pub fn separate_storages_get_different_ids<F: IdentityFixture>(f: &mut F) {
    let a = f.empty();
    let b = f.empty();
    assert_ne!(id(&a), id(&b));
}

pub fn the_id_is_64_lowercase_hex_digits<F: IdentityFixture>(f: &mut F) {
    let identity = f.empty();
    let id = id(&identity);
    let text = id.as_str();
    assert_eq!(text.len(), 64, "{text}");
    assert!(
        text.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{text}"
    );
}

pub fn a_stored_key_is_loaded_and_kept<F: IdentityFixture>(f: &mut F) {
    let key = [7u8; 32];
    let identity = f.holding(&key);
    let first = id(&identity);
    assert_eq!(f.stored(&identity), Some(key.to_vec()));
    assert_eq!(id(&f.reopen(&identity)), first);
}

pub fn the_id_depends_only_on_the_key<F: IdentityFixture>(f: &mut F) {
    let same_a = id(&f.holding(&[1; 32]));
    let same_b = id(&f.holding(&[1; 32]));
    let other = id(&f.holding(&[2; 32]));
    assert_eq!(same_a, same_b);
    assert_ne!(same_a, other);
}

pub fn a_stored_key_of_the_wrong_size_is_an_error_and_kept<F: IdentityFixture>(f: &mut F) {
    for len in [0usize, 1, 31, 33, 64] {
        let bytes = vec![9u8; len];
        let identity = f.holding(&bytes);
        let result = identity.load_or_create().map(|k| k.machine_id());
        assert_eq!(
            result,
            Err(IdentityError::WrongSize { len: len as u64 }),
            "{len} bytes"
        );
        assert_eq!(f.stored(&identity), Some(bytes), "{len} bytes are kept");
    }
}

pub fn a_key_others_can_read_is_insecure_and_kept<F: IdentityFixture>(f: &mut F) {
    for mode in [0o640, 0o604, 0o644] {
        let key = [3u8; 32];
        let identity = f.holding_with_mode(&key, mode);
        let result = identity.load_or_create().map(|k| k.machine_id());
        assert_eq!(
            result,
            Err(IdentityError::Insecure { mode }),
            "mode {mode:o}"
        );
        assert_eq!(f.stored(&identity), Some(key.to_vec()), "mode {mode:o}");
    }
}

/// Mode 0: not even the user can read it (a test running as root reads it anyway, so the real adapter's fixture
/// must not run as root).
pub fn a_key_that_cannot_be_read_is_an_io_error<F: IdentityFixture>(f: &mut F) {
    let identity = f.holding_with_mode(&[4; 32], 0o000);
    let result = identity.load_or_create().map(|k| k.machine_id());
    assert!(matches!(result, Err(IdentityError::Io(_))), "{result:?}");
}

pub fn something_other_than_a_file_is_the_wrong_type<F: IdentityFixture>(f: &mut F) {
    let identity = f.not_a_file();
    let result = identity.load_or_create().map(|k| k.machine_id());
    assert_eq!(result, Err(IdentityError::WrongType));
    // Still not a file: nothing was created in its place.
    assert_eq!(f.stored(&identity), None);
    let again = identity.load_or_create().map(|k| k.machine_id());
    assert_eq!(again, Err(IdentityError::WrongType));
}
