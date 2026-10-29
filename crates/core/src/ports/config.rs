//! Where the config comes from. A source only reads and parses; merging and validation are
//! [`Config::resolve`](crate::config::Config::resolve)'s job.
//!
//! The runtime loads the config once, when the host starts. There is no hot reload.

pub use crate::config::{ConfigError, RawConfig};

pub trait ConfigSource {
    /// Reads the config as written. `Ok(None)` means there is no config (e.g. the file does not exist), which
    /// resolves to the built-in defaults. A source that cannot be parsed is [`ConfigError::Invalid`] with the
    /// position of the problem; one that cannot be read is [`ConfigError::Io`].
    fn load(&self) -> Result<Option<RawConfig>, ConfigError>;
}
