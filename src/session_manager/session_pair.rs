use std::path::PathBuf;

use crate::session::{AttachedSession, DetachedSession};

/// Activity status of a Claude session (for hook notifications)
#[derive(Clone, PartialEq, Default)]
pub enum SessionActivity {
    /// Claude is working (default state)
    #[default]
    Active,
    /// Claude is currently running a tool
    RunningTool(String),
    /// Claude stopped and needs user attention
    Stopped,
}

/// An active session pair - claude session is attached (can receive input)
pub struct ActivePair {
    pub name: String,
    pub path: PathBuf,
    pub claude: AttachedSession,
    /// Whether this session was started via resume (--continue flag)
    pub resumed: bool,
    /// Scroll offset for viewing scrollback history (0 = at bottom, showing current output)
    pub scroll_offset: usize,
    /// Activity status from hook notifications
    pub activity: SessionActivity,
}

impl ActivePair {
    pub fn new(name: String, path: PathBuf, claude: AttachedSession, resumed: bool) -> Self {
        Self {
            name,
            path,
            claude,
            resumed,
            scroll_offset: 0,
            activity: SessionActivity::Active,
        }
    }

    pub fn detach(self) -> BackgroundPair {
        BackgroundPair {
            name: self.name,
            path: self.path,
            claude: self.claude.detach(),
            resumed: self.resumed,
            scroll_offset: self.scroll_offset,
            activity: self.activity,
        }
    }
}

/// A background session pair - claude session is detached
pub struct BackgroundPair {
    pub name: String,
    pub path: PathBuf,
    pub claude: DetachedSession,
    /// Whether this session was started via resume (--continue flag)
    pub resumed: bool,
    /// Scroll offset for viewing scrollback history (0 = at bottom, showing current output)
    pub scroll_offset: usize,
    /// Activity status from hook notifications
    pub activity: SessionActivity,
}

impl BackgroundPair {
    pub fn attach(self) -> anyhow::Result<ActivePair> {
        Ok(ActivePair {
            name: self.name,
            path: self.path,
            claude: self.claude.attach()?,
            resumed: self.resumed,
            scroll_offset: self.scroll_offset,
            // Preserve activity state - only cleared when user sends input
            activity: self.activity,
        })
    }
}
