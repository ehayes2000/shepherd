//! `ConfigSource` reading a TOML file, normally `~/.shepherd/config.toml`.

use std::io::ErrorKind;
use std::path::PathBuf;

use shepherd_core::ports::{ConfigError, ConfigSource, RawConfig};

pub struct TomlConfigSource {
    path: PathBuf,
}

impl TomlConfigSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ConfigSource for TomlConfigSource {
    fn load(&self) -> Result<Option<RawConfig>, ConfigError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ConfigError::Io(format!("{}: {e}", self.path.display()))),
        };
        toml::from_str(&text).map(Some).map_err(|e| {
            let (line, column) = e
                .span()
                .map_or((1, 1), |span| line_column(&text, span.start));
            ConfigError::Invalid {
                line,
                column,
                message: e.message().to_owned(),
            }
        })
    }
}

/// The 1-based line and character column of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use shepherd_testkit::contracts::config::{ConfigFixture, Doc, TextConfigFixture};
    use tempfile::TempDir;

    use super::TomlConfigSource;

    /// Each source gets its own file in one temp dir, which lives as long as the fixture (one test).
    struct TomlFixture {
        dir: TempDir,
        next: Cell<u32>,
    }

    impl TomlFixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                next: Cell::new(0),
            }
        }

        fn fresh_path(&self) -> std::path::PathBuf {
            let n = self.next.get();
            self.next.set(n + 1);
            self.dir.path().join(format!("config-{n}.toml"))
        }
    }

    impl ConfigFixture for TomlFixture {
        type Source = TomlConfigSource;

        fn missing(&self) -> TomlConfigSource {
            TomlConfigSource::new(self.fresh_path())
        }

        /// A directory where the file should be.
        fn unreadable(&self) -> TomlConfigSource {
            let path = self.fresh_path();
            std::fs::create_dir(&path).unwrap();
            TomlConfigSource::new(path)
        }

        fn holding(&self, doc: &Doc) -> TomlConfigSource {
            self.text(doc.toml)
        }
    }

    impl TextConfigFixture for TomlFixture {
        fn text(&self, text: &str) -> TomlConfigSource {
            let path = self.fresh_path();
            std::fs::write(&path, text).unwrap();
            TomlConfigSource::new(path)
        }
    }

    shepherd_testkit::config_contract!(text: TomlFixture::new());
}
