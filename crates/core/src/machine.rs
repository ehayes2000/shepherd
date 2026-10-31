//! What a machine is called (T29): `[machine] name` from the config, else its hostname, else [`UNNAMED`]. A name is
//! display-safe ([`display_safe`]), trimmed, not empty, and at most [`MAX_NAME`] grapheme clusters: one the config
//! gives that is empty or longer is a config error, while a hostname is cut to fit.
//!
//! The host records the name it starts with in `_shepherd/machine/renamed` when it differs from the last one recorded
//! (see [`crate::host::Host::machine_name`]), and tells clients in its hello.

use unicode_segmentation::UnicodeSegmentation;

use crate::config::{Config, ConfigError};
use crate::display::display_safe;

/// The most grapheme clusters a machine name has.
pub const MAX_NAME: usize = 64;

/// What a machine is called when neither the config nor its hostname names it.
pub const UNNAMED: &str = "this machine";

/// `text` made display-safe and trimmed, all of it (the config and hostnames are short, local text).
fn safe(text: &str) -> String {
    display_safe(text.trim(), text.len())
        .as_str()
        .trim()
        .to_owned()
}

/// `[machine] name` as the config resolves it: display-safe and trimmed, then refused if empty or too long.
pub(crate) fn configured(name: &str) -> Result<String, ConfigError> {
    let name = safe(name);
    match name.graphemes(true).count() {
        0 => Err(ConfigError::EmptyMachineName),
        n if n > MAX_NAME => Err(ConfigError::LongMachineName { max: MAX_NAME }),
        _ => Ok(name),
    }
}

/// What this machine is called: `config`'s `[machine] name`, else `hostname` (as the system reports it) made
/// display-safe and cut to [`MAX_NAME`] grapheme clusters, else [`UNNAMED`].
pub fn name(config: &Config, hostname: Option<&str>) -> String {
    if let Some(name) = config.machine_name() {
        return name.to_owned();
    }
    let host = hostname.map(safe).unwrap_or_default();
    let host = display_safe(&host, MAX_NAME).as_str().trim_end().to_owned();
    if host.is_empty() {
        return UNNAMED.to_owned();
    }
    host
}

/// What a peer is recorded as being called when the name it gives has nothing to show.
pub const UNNAMED_PEER: &str = "unnamed machine";

/// The name a peer gives itself (untrusted text from the network, T30), as it is recorded and shown: display-safe,
/// trimmed and cut to [`MAX_NAME`] grapheme clusters, else [`UNNAMED_PEER`].
pub fn peer_name(claimed: &str) -> String {
    let name = safe(claimed);
    let name = display_safe(&name, MAX_NAME).as_str().trim_end().to_owned();
    if name.is_empty() {
        return UNNAMED_PEER.to_owned();
    }
    name
}
