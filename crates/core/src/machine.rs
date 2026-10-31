//! What a machine is called (T29): `[machine] name` from the config, else its hostname, else [`UNNAMED`]. A name is
//! display-safe ([`display_safe`]), trimmed, not empty, and at most [`MAX_NAME`] grapheme clusters: one the config
//! gives that is empty or longer is a config error, while a hostname is cut to fit.
//!
//! The host records the name it starts with in `_shepherd/machine/renamed` when it differs from the last one recorded
//! (see [`crate::host::Host::machine_name`]), and tells clients in its hello.
//!
//! What color a machine is drawn in (T62) is [`MachineColor`].

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::{Config, ConfigError};
use crate::display::display_safe;
use crate::ids::MachineId;

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

/// What color a machine's name is drawn in, wherever it appears (T62). One of a fixed palette of named terminal
/// colors, none of them the default background or the selection's: [`MachineColor::of`] its id unless its host's config
/// gives one (`[machine] color`, which its hello carries). The TUI maps each to a terminal color. On the wire and in the
/// config by its [`as_str`](Self::as_str) name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MachineColor {
    Cyan,
    Magenta,
    Blue,
    LightCyan,
    LightMagenta,
    LightBlue,
}

impl MachineColor {
    /// The palette, in the order [`MachineColor::of`] indexes it. Any change to it recolors most machines.
    pub const ALL: [Self; 6] = [
        Self::Cyan,
        Self::Magenta,
        Self::Blue,
        Self::LightCyan,
        Self::LightMagenta,
        Self::LightBlue,
    ];

    /// The color `machine` gets when its host doesn't say: the first 8 bytes of the SHA-256 of its id, big-endian,
    /// modulo the palette. Stable across builds and platforms, so every client agrees.
    pub fn of(machine: &MachineId) -> Self {
        let digest = Sha256::digest(machine.as_str().as_bytes());
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        let len = Self::ALL.len() as u64;
        let index = usize::try_from(u64::from_be_bytes(head) % len).unwrap_or(0);
        Self::ALL[index]
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cyan => "cyan",
            Self::Magenta => "magenta",
            Self::Blue => "blue",
            Self::LightCyan => "light-cyan",
            Self::LightMagenta => "light-magenta",
            Self::LightBlue => "light-blue",
        }
    }
}

/// `[machine] color` as the config resolves it: a palette color's name.
pub(crate) fn configured_color(name: &str) -> Result<MachineColor, ConfigError> {
    MachineColor::parse(name).ok_or_else(|| ConfigError::UnknownMachineColor(name.to_owned()))
}
