//! The `ConfigSource` contract.
//!
//! Sources come in two shapes. Every source can be missing, unreadable, or hold a config: those cases run through a
//! [`ConfigFixture`] and apply to the fake too, which holds a `RawConfig` directly. A source that parses text (the
//! TOML file adapter) also implements [`TextConfigFixture`], which adds the parsing cases: syntax errors with their
//! position, unknown keys, wrong types. The fake has no text to parse, so it runs only the first set.
//!
//! Run it with [`config_contract!`](crate::config_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `ConfigSource` contract case. `$fixture` is an expression building a fresh
/// [`ConfigFixture`] for each test:
///
/// - `config_contract!(FakeFixture)` runs the cases every source must pass.
/// - `config_contract!(text: TomlFixture::new())` also runs the cases for sources that parse text; the fixture
///   must implement [`TextConfigFixture`].
///
/// New cases go in the lists below.
#[macro_export]
macro_rules! config_contract {
    (text: $fixture:expr) => {
        $crate::config_contract!($fixture);
        $crate::config_contract!(@cases $fixture;
            syntax_error_is_invalid_with_line_and_column,
            error_column_counts_characters_not_bytes,
            unknown_top_level_key_is_invalid,
            unknown_agent_field_is_invalid,
            wrong_type_is_invalid_with_position,
        );
    };
    ($fixture:expr) => {
        $crate::config_contract!(@cases $fixture;
            missing_source_is_none,
            unreadable_source_is_io_error,
            empty_config_loads_as_default_raw_config,
            full_config_loads_every_field,
            load_is_repeatable,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::config::$case(&$fixture);
            }
        )*
    };
}

use std::collections::BTreeMap;

use shepherd_core::config::{RawAgent, RawDefaults, RawKeys, RawNotify};
use shepherd_core::ports::{ConfigError, ConfigSource, RawConfig};

/// A config both as text and as the `RawConfig` it parses to.
pub struct Doc {
    pub toml: &'static str,
    pub raw: RawConfig,
}

/// Builds sources in each state the contract needs.
pub trait ConfigFixture {
    type Source: ConfigSource;
    /// A source with no config at all (for a file: the file does not exist).
    fn missing(&self) -> Self::Source;
    /// A source that exists but cannot be read.
    fn unreadable(&self) -> Self::Source;
    /// A source holding `doc`, either as its text or as its `RawConfig`.
    fn holding(&self, doc: &Doc) -> Self::Source;
}

/// A fixture for sources that parse text.
pub trait TextConfigFixture: ConfigFixture {
    fn text(&self, text: &str) -> Self::Source;
}

pub fn empty_doc() -> Doc {
    Doc {
        toml: "",
        raw: RawConfig::default(),
    }
}

/// Every field the file supports, set.
pub fn full_doc() -> Doc {
    let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    Doc {
        toml: r#"
[defaults]
agent = "yolo"
env = { TERM_PROGRAM = "shepherd" }

[agents.claude]
args = ["--verbose"]

[agents.yolo]
kind = "claude-code"
command = "claude"
args = ["--dangerously-skip-permissions"]
env = { A = "1", B = "2" }

[keys]
prefix = "ctrl-a"

[notify]
bell = true
"#,
        raw: RawConfig {
            defaults: Some(RawDefaults {
                agent: Some("yolo".into()),
                env: Some(map(&[("TERM_PROGRAM", "shepherd")])),
            }),
            agents: Some(BTreeMap::from([
                (
                    "claude".into(),
                    RawAgent {
                        args: Some(vec!["--verbose".into()]),
                        ..RawAgent::default()
                    },
                ),
                (
                    "yolo".into(),
                    RawAgent {
                        kind: Some("claude-code".into()),
                        command: Some("claude".into()),
                        args: Some(vec!["--dangerously-skip-permissions".into()]),
                        env: Some(map(&[("A", "1"), ("B", "2")])),
                        idle_after_quiet_ms: None,
                    },
                ),
            ])),
            keys: Some(RawKeys {
                prefix: Some("ctrl-a".into()),
            }),
            notify: Some(RawNotify { bell: Some(true) }),
        },
    }
}

/// Asserts `result` is `Invalid` at `line`, `column`.
fn assert_invalid_at(result: Result<Option<RawConfig>, ConfigError>, line: usize, column: usize) {
    match result {
        Err(ConfigError::Invalid {
            line: l,
            column: c,
            message,
        }) => {
            assert_eq!((l, c), (line, column), "wrong position; message: {message}");
            assert!(!message.is_empty(), "Invalid needs a message");
        }
        other => panic!("expected Invalid at {line}:{column}, got {other:?}"),
    }
}

// ---------- every source ----------

pub fn missing_source_is_none<F: ConfigFixture>(f: &F) {
    assert_eq!(f.missing().load(), Ok(None));
}

pub fn unreadable_source_is_io_error<F: ConfigFixture>(f: &F) {
    let result = f.unreadable().load();
    assert!(matches!(result, Err(ConfigError::Io(_))), "got {result:?}");
}

pub fn empty_config_loads_as_default_raw_config<F: ConfigFixture>(f: &F) {
    let doc = empty_doc();
    assert_eq!(f.holding(&doc).load(), Ok(Some(doc.raw)));
}

pub fn full_config_loads_every_field<F: ConfigFixture>(f: &F) {
    let doc = full_doc();
    assert_eq!(f.holding(&doc).load(), Ok(Some(doc.raw)));
}

pub fn load_is_repeatable<F: ConfigFixture>(f: &F) {
    let doc = full_doc();
    let source = f.holding(&doc);
    assert_eq!(source.load(), Ok(Some(doc.raw.clone())));
    assert_eq!(source.load(), Ok(Some(doc.raw)));
}

// ---------- sources that parse text ----------

pub fn syntax_error_is_invalid_with_line_and_column<F: TextConfigFixture>(f: &F) {
    let text = "[defaults]\nagent = \"claude\"\n\n[keys]\nprefix = = \"ctrl-a\"\n";
    assert_invalid_at(f.text(text).load(), 5, 10);
}

pub fn error_column_counts_characters_not_bytes<F: TextConfigFixture>(f: &F) {
    let text = "[defaults]\nagent = \"ünïcödé\" junk\n";
    assert_invalid_at(f.text(text).load(), 2, 19);
}

/// Typos must not be silently ignored.
pub fn unknown_top_level_key_is_invalid<F: TextConfigFixture>(f: &F) {
    let text = "[keys]\nprefix = \"ctrl-a\"\n\n[default]\nagent = \"codex\"\n";
    assert_invalid_at(f.text(text).load(), 4, 2);
}

pub fn unknown_agent_field_is_invalid<F: TextConfigFixture>(f: &F) {
    let text = "[agents.claude]\ncomand = \"claude\"\n";
    assert_invalid_at(f.text(text).load(), 2, 1);
}

pub fn wrong_type_is_invalid_with_position<F: TextConfigFixture>(f: &F) {
    let text = "[keys]\nprefix = 3\n";
    assert_invalid_at(f.text(text).load(), 2, 10);
}
