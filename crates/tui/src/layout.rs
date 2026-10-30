//! Where things go on the window: the sidebar on the left, the selected session's terminal (the pane) on the right.
//!
//! The pane is the whole height of the window, so the size a session is resized to ([`pane_size`]) is exactly the
//! area [`crate::render`] draws its screen in.

use ratatui::layout::Rect;
use shepherd_core::ports::pty::PtySize;

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

/// Splits `area`. The pane always keeps at least one column: a window too narrow for the sidebar has none.
pub fn layout(area: Rect) -> Layout {
    let collapsed = area.width < COLLAPSE_BELOW;
    let wanted = if collapsed {
        COLLAPSED_WIDTH
    } else {
        SIDEBAR_WIDTH
    };
    let side = if area.width > wanted { wanted } else { 0 };
    Layout {
        sidebar: Rect::new(area.x, area.y, side, area.height),
        pane: Rect::new(area.x + side, area.y, area.width - side, area.height),
        collapsed,
    }
}

/// The size of the pane in a `cols` x `rows` window: what the selected session is resized to. Never smaller than
/// 1 x 1, which only matters for an empty window.
pub fn pane_size(cols: u16, rows: u16) -> PtySize {
    let pane = layout(Rect::new(0, 0, cols, rows)).pane;
    PtySize {
        cols: pane.width.max(1),
        rows: pane.height.max(1),
    }
}
