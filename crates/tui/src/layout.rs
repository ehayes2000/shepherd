//! Where things go on the window: the sidebar on the left, the pane area on the right, where the host's layout is drawn
//! (its page bar and panes, see `panes`).
//!
//! The pane area is the whole height of the window. Its size ([`pane_size`]) is what the client arranges the layout
//! in, the same way [`crate::render`] does, so each session is resized to exactly the area its screen is drawn in.

use std::path::PathBuf;

use ratatui::layout::{Position, Rect};
use shepherd_core::client::panes::Divider;
use shepherd_core::client::{Client, FinderItem, MenuItem, PaneMenuItem, SidebarEntry};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{PageId, PaneId};
use shepherd_core::ports::pty::PtySize;
use shepherd_term::{MIN_COLS, MIN_ROWS};

use crate::{panes, popup, sidebar};

/// The sidebar's width, its right border included.
pub const SIDEBAR_WIDTH: u16 = 24;
/// The collapsed sidebar's width: an attention mark, a glyph, and the border.
pub const COLLAPSED_WIDTH: u16 = 3;
/// Windows narrower than this collapse the sidebar to glyphs.
pub const COLLAPSE_BELOW: u16 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub sidebar: Rect,
    pub pane: Rect,
    /// The sidebar shows only status glyphs.
    pub collapsed: bool,
}

/// Splits `area`. The pane always keeps [`MIN_COLS`] (the narrowest screen the emulator holds): a window too narrow
/// for the sidebar and that has none.
pub fn layout(area: Rect) -> Layout {
    let collapsed = area.width < COLLAPSE_BELOW;
    let wanted = if collapsed {
        COLLAPSED_WIDTH
    } else {
        SIDEBAR_WIDTH
    };
    let side = if area.width >= wanted + MIN_COLS {
        wanted
    } else {
        0
    };
    Layout {
        sidebar: Rect::new(area.x, area.y, side, area.height),
        pane: Rect::new(area.x + side, area.y, area.width - side, area.height),
        collapsed,
    }
}

/// A control the mouse can click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The sidebar's search field, on its first line (`⌕` when collapsed): opens the finder.
    Search,
    /// The sidebar's workspace, on its second line (`⌂` when collapsed): opens the switcher.
    Workspace,
    /// The sidebar's `+ New session` button (`+` when collapsed).
    NewSession,
    /// A session's row in the sidebar, either of its lines.
    Session(SessionId),
    /// A machine's header in the sidebar, either of its lines (while there are peers).
    Machine(MachineId),
    /// The `×` at the right of a session's row.
    CloseSession(SessionId),
    /// A session's subagent badge (`▸N` / `▾N`): expands or collapses its tree.
    Subagents(SessionId),
    /// A subagent's line in an expanded tree.
    Subagent { session: SessionId, id: String },
    /// The focused pane's terminal (the whole pane area with one pane).
    Terminal,
    /// A pane other than the focused one, or a focused pane's border: a click focuses it.
    Pane(PaneId),
    /// A split pane's menu button, `[≡]` at the right of its top edge.
    PaneMenu(PaneId),
    /// An item of the open pane menu.
    PaneMenuItem(PaneMenuItem),
    /// A page's name in the page bar.
    Page(PageId),
    /// The page bar's `+`.
    NewPage,
    /// The sidebar's `⊞ Layout` (`⊞` when collapsed): opens the focused pane's menu.
    LayoutMenu,
    /// Where two split panes meet: dragged, it sets the split's ratio.
    Divider(Divider),
    /// The sidebar footer's `Detach` button (`⏏` when collapsed).
    Detach,
    /// The sidebar footer's `?` button.
    Help,
    /// An item of the open context menu.
    MenuItem(MenuItem),
    /// A profile's row in the open profile picker.
    Profile(String),
    /// A workspace's row in the open workspace switcher.
    WorkspaceRow(PathBuf),
    /// A machine's row in the open new-session popup or workspace switcher (while there are peers). Only a machine
    /// that can be reached now has one.
    MachineRow(MachineId),
    /// A result's row in the open finder.
    FinderRow(FinderItem),
    /// The open dialog's button that answers yes: `[Close]`, `[Save]`, `[Open]` or `[Start session]`.
    Confirm,
    /// The open dialog's button that cancels it: `[Cancel]`, or help's `[Close]`.
    Dismiss,
    /// The close confirmation's `[Remove anyway]`, for a worktree that would lose work.
    RemoveWorktree,
    /// The close confirmation's `[Keep worktree]`, for a worktree that would lose work.
    KeepWorktree,
    /// The restart dialog's `[Restart host]`.
    RestartHost,
    /// The sidebar footer's `Restart host`, while the host should be restarted: reopens the restart dialog.
    OfferRestart,
    /// The sidebar footer's `Host warning`, while the host warns about something: shows it again.
    ShowHostWarning,
    /// The sidebar footer's `Cleanup`: opens the cleanup view.
    Cleanup,
    /// The cleanup view's `[Remove safe]`.
    CleanupRemoveSafe,
    /// The open popup, off its controls.
    Popup,
    /// Anywhere outside the open popup.
    Backdrop,
}

/// What is under the cell at `col`, `row`, from the same geometry and scroll [`crate::render`] draws with. An open
/// popup covers the whole window: its controls, itself, or the backdrop around it.
pub fn hit(layout: &Layout, client: &Client, col: u16, row: u16) -> Option<Target> {
    let at = Position::new(col, row);
    if let Some(placed) = popup::place(client, layout.sidebar.union(layout.pane)) {
        let control = placed.controls.into_iter().find(|c| c.area.contains(at));
        return Some(match control {
            Some(control) => control.target,
            None if placed.area.contains(at) => Target::Popup,
            None => Target::Backdrop,
        });
    }
    if layout.pane.contains(at) {
        return panes::hit(client, layout.pane, col, row);
    }
    if sidebar::search_field(layout).contains(at) {
        return Some(Target::Search);
    }
    if sidebar::workspace_button(client, layout).contains(at) {
        return Some(Target::Workspace);
    }
    if sidebar::new_session_button(layout).contains(at) {
        return Some(Target::NewSession);
    }
    if sidebar::layout_button(layout).contains(at) {
        return Some(Target::LayoutMenu);
    }
    let footer = sidebar::footer_buttons(client, layout);
    if let Some((_, target, _)) = footer.into_iter().find(|(area, ..)| area.contains(at)) {
        return Some(target);
    }
    let (entry, line) = sidebar::visible_rows(client, layout)
        .into_iter()
        .find(|(_, line)| line.contains(at))?;
    let session = match entry {
        SidebarEntry::Session(session) => session,
        SidebarEntry::Machine(group) => return Some(Target::Machine(group.id)),
        SidebarEntry::Subagent {
            parent, subagent, ..
        } => {
            return Some(Target::Subagent {
                session: parent.clone(),
                id: subagent.id().to_owned(),
            });
        }
    };
    let id = session.id.clone();
    if sidebar::close_button(layout, line).contains(at) {
        return Some(Target::CloseSession(id));
    }
    if sidebar::subagent_badge(client, layout, session, line).contains(at) {
        return Some(Target::Subagents(id));
    }
    Some(Target::Session(id))
}

/// How many sidebar entries are sure to fit at once in a `window`, for [`Intent::SidebarRows`].
///
/// [`Intent::SidebarRows`]: shepherd_core::client::Intent::SidebarRows
pub fn sidebar_rows(window: Rect) -> usize {
    sidebar::capacity(&layout(window))
}

/// The size of the pane in a `cols` x `rows` window: what the selected session is resized to. Never smaller than the
/// emulator's smallest screen ([`MIN_COLS`] x [`MIN_ROWS`]), which only matters for a window smaller than that.
pub fn pane_size(cols: u16, rows: u16) -> PtySize {
    let pane = layout(Rect::new(0, 0, cols, rows)).pane;
    PtySize {
        cols: pane.width.max(MIN_COLS),
        rows: pane.height.max(MIN_ROWS),
    }
}
