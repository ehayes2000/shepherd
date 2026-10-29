//! The synchronized-update timeout, injected into the parser with no clock.
//!
//! alacritty's parser arms a timeout when it sees `CSI ? 2026 h` and keeps buffering while one is pending. Its
//! standard handler reads `Instant::now()`. This one only records that a timeout was asked for; the caller owns the
//! clock and ends the update through `Screen::expire_sync`.

use std::time::Duration;

use alacritty_terminal::vte::ansi::Timeout;

use crate::SyncHold;

#[derive(Default)]
pub(crate) struct ManualTimeout {
    /// Bumped on every arm, so a timer for an earlier begin marker can be told apart.
    generation: u64,
    armed: Option<Duration>,
}

impl ManualTimeout {
    pub(crate) fn hold(&self) -> Option<SyncHold> {
        self.armed.map(|timeout| SyncHold {
            id: self.generation,
            timeout,
        })
    }
}

impl Timeout for ManualTimeout {
    fn set_timeout(&mut self, duration: Duration) {
        self.generation = self.generation.wrapping_add(1);
        self.armed = Some(duration);
    }

    fn clear_timeout(&mut self) {
        self.armed = None;
    }

    fn pending_timeout(&self) -> bool {
        self.armed.is_some()
    }
}
