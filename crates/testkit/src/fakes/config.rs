//! An in-memory `ConfigSource` holding a `RawConfig` (or nothing, or an error) instead of a file. It parses no
//! text, so it runs only the source-independent part of the contract (see `contracts::config`).

use shepherd_core::ports::{ConfigError, ConfigSource, RawConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeConfigSource {
    result: Result<Option<RawConfig>, ConfigError>,
}

impl FakeConfigSource {
    /// No config: resolves to the built-in defaults.
    pub fn missing() -> Self {
        Self { result: Ok(None) }
    }

    pub fn with(raw: RawConfig) -> Self {
        Self {
            result: Ok(Some(raw)),
        }
    }

    /// Every `load` fails with `error`.
    pub fn failing(error: ConfigError) -> Self {
        Self { result: Err(error) }
    }

    /// Replaces what the next `load` returns, as if the file changed between host starts.
    pub fn set(&mut self, raw: Option<RawConfig>) {
        self.result = Ok(raw);
    }
}

impl ConfigSource for FakeConfigSource {
    fn load(&self) -> Result<Option<RawConfig>, ConfigError> {
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::config::{ConfigFixture, Doc};

    struct FakeFixture;

    impl ConfigFixture for FakeFixture {
        type Source = FakeConfigSource;

        fn missing(&self) -> FakeConfigSource {
            FakeConfigSource::missing()
        }

        fn unreadable(&self) -> FakeConfigSource {
            FakeConfigSource::failing(ConfigError::Io("permission denied".into()))
        }

        fn holding(&self, doc: &Doc) -> FakeConfigSource {
            FakeConfigSource::with(doc.raw.clone())
        }
    }

    mod contract {
        use super::FakeFixture;

        crate::config_contract!(FakeFixture);
    }

    #[test]
    fn failing_source_returns_its_error_on_every_load() {
        let error = ConfigError::Invalid {
            line: 3,
            column: 7,
            message: "expected `=`".into(),
        };
        let source = FakeConfigSource::failing(error.clone());
        assert_eq!(source.load(), Err(error.clone()));
        assert_eq!(source.load(), Err(error));
    }

    #[test]
    fn set_changes_what_the_next_load_returns() {
        let mut source = FakeConfigSource::missing();
        source.set(Some(RawConfig::default()));
        assert_eq!(source.load(), Ok(Some(RawConfig::default())));
    }
}
