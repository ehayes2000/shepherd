//! The size each session's terminal was last given, so its PTY spawns at that size instead of the one its spec was
//! planned with.
//!
//! A client resizes the session it shows as soon as it selects it. For a new worktree session that is before its PTY
//! exists (its worktree and post-create hooks come first), and for an exited one before it is resumed, so
//! [`Host::handle`] has no PTY to resize and drops it. [`Sizes`] keeps the latest such size for every open session,
//! running or not, and every [`HostCommand::SpawnPty`] for that session spawns at it. A session with no known size
//! spawns at its spec's size: what its `session/new` asked for, else the runtime's default.
//!
//! Not host state: resizes aren't logged, so a size is known only to this host run (and to the next image, for the
//! sessions a swap carries: [`Sizes::adopted`]). The runtime keeps one [`Sizes`] and runs every event through
//! [`Sizes::handle`] instead of calling [`Host::handle`] itself. Sizes of closed sessions are kept, like the host's own
//! entries for them: one small entry per session ever created in this run.

use std::collections::BTreeMap;

use super::{Host, HostCommand, HostEvent};
use crate::ids::SessionId;
use crate::ports::pty::PtySize;

/// The latest known terminal size of each session. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sizes {
    known: BTreeMap<SessionId, PtySize>,
}

impl Sizes {
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest size known for `session`, if any.
    pub fn get(&self, session: &SessionId) -> Option<PtySize> {
        self.known.get(session).copied()
    }

    /// `session`'s PTY was carried over from the previous image at `size`.
    pub fn adopted(&mut self, session: SessionId, size: PtySize) {
        self.known.insert(session, size);
    }

    /// [`Host::handle`] for `event`, with what it says about sizes: a resize of an open session is its latest size,
    /// and every PTY spawns at the latest size known for its session.
    pub fn handle(&mut self, host: &Host, event: HostEvent) -> Vec<HostCommand> {
        if let HostEvent::Resize { session, size } = &event
            && host.is_open(session)
        {
            self.known.insert(session.clone(), *size);
        }
        let mut commands = host.handle(event);
        for command in &mut commands {
            if let HostCommand::SpawnPty { session, spec } = command
                && let Some(size) = self.get(session)
            {
                spec.size = size;
            }
        }
        commands
    }
}
