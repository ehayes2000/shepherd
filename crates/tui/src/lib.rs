//! Pure rendering of client state and key mapping. See `docs/architecture.md`.
//!
//! - [`render`] draws the sidebar ([`sidebar`]), the host's layout in the pane area ([`layout`]): its page bar and
//!   panes, each showing its session's terminal (`panes`) or the status page (`status`), and any popup or menu. [`cursor_style`] is the cursor shape to go with it, which a ratatui frame can't carry.
//! - [`map_event`] / [`map_key`] turn terminal events into client [`Intent`](shepherd_core::client::Intent)s, with input
//!   for the session encoded by [`encode`] for its [`Screen::modes`].
//!
//! # Status glyphs
//!
//! | session                          | glyph | colour     |
//! |----------------------------------|-------|------------|
//! | starting (no turn or idle yet)   | `◌`   | dark gray  |
//! | working                          | `●`   | green      |
//! | needs permission / needs input   | `▲`   | yellow     |
//! | idle                             | `○`   | default    |
//! | exited                           | `✕`   | red        |
//! | unknown                          | `·`   | dark gray  |
//!
//! A session one of whose subagents is blocked on the user shows `▲` whatever the agent itself is doing (unless it
//! exited).
//!
//! A session that wants the user (blocked on them, or idle since they last looked) also gets a yellow `▌` on the
//! left edge of its row, in both the full and the collapsed sidebar.

mod cleanup;
mod draw;
pub mod encode;
mod finder;
mod keys;
pub mod layout;
pub mod machine;
mod panes;
mod popup;
mod screen;
pub mod sidebar;
mod status;
mod width;

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::Wrap;
use shepherd_core::client::{Client, Connection, Focus};
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{LayoutOp, PageId};
use shepherd_core::ports::pty::PtySize;
use shepherd_term::{CursorShape, Screen};

use draw::shown;
pub use keys::{PREFIX, Pointer, from_legacy_terminal, map_event, map_key};
pub use layout::{Layout, Target, hit, layout, pane_size};
pub use popup::BINDINGS;
pub use screen::render_screen;

/// Each session's terminal, as the client runtime keeps them.
pub type Screens = BTreeMap<SessionId, Screen>;

/// The size session `id`'s terminal is made at, on its first output: the size the client last gave the session (its
/// pane's terminal), else the client's view size, if it knows either.
pub fn screen_size(client: &Client, id: &SessionId) -> Option<PtySize> {
    client.session_size(id).or(client.size())
}

/// Resizes each terminal in `screens` to the size the client last gave its session (its pane's terminal, T49), so it
/// matches the session's PTY. The client runtime calls this after every event.
pub fn fit_screens(client: &Client, screens: &mut Screens) {
    for (id, screen) in screens.iter_mut() {
        if let Some(size) = client.session_size(id)
            && screen.size() != (size.cols, size.rows)
        {
            screen.resize(size.cols, size.rows);
        }
    }
}

/// Draws the whole window.
pub fn render(client: &Client, screens: &Screens, frame: &mut Frame) {
    let popup_cursor = draw_window(client, screens, &client.host_layout(), frame);
    let layout = layout(frame.area());
    let terminal = panes::focused_terminal(client, layout.pane);
    let cursor = match (client.popup(), cursor_session(client, screens), terminal) {
        (Some(_), ..) => popup_cursor,
        (None, Some(screen), Some(terminal)) => session_cursor(screen, terminal),
        (None, ..) => None,
    };
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

/// Draws the whole window as [`render`] does, but with page `page` of `layout` in the pane area instead of the host's
/// shown page: what the window would show were that page selected, for a headless screenshot (T50). Nothing changes,
/// and no cursor is placed. A page `layout` lacks draws its shown page.
pub fn render_page(
    client: &Client,
    screens: &Screens,
    layout: &shepherd_core::layout::Layout,
    page: PageId,
    frame: &mut Frame,
) {
    let shown = layout
        .apply(&LayoutOp::PageSelect { page }, |_| true)
        .unwrap_or_else(|_| layout.clone());
    draw_window(client, screens, &shown, frame);
}

/// Draws the sidebar, `shown` in the pane area, and any notice or popup. Returns where the popup wants the cursor.
fn draw_window(
    client: &Client,
    screens: &Screens,
    shown: &shepherd_core::layout::Layout,
    frame: &mut Frame,
) -> Option<Position> {
    let layout = layout(frame.area());
    let buf = frame.buffer_mut();
    sidebar::render(client, &layout, buf);
    panes::render(client, screens, shown, layout.pane, buf);
    sidebar::wide_notice(client, &layout, buf);
    popup::render(client, frame.area(), frame.buffer_mut())
}

/// The session whose cursor [`render`] shows: the focused pane's, while keys go to it and no popup is open.
fn cursor_session<'a>(client: &Client, screens: &'a Screens) -> Option<&'a Screen> {
    if client.popup().is_some() || client.focus() != Focus::Terminal {
        return None;
    }
    screens.get(&client.focused_session()?)
}

/// The shape and blink of the real terminal's cursor, for the runtime to set after each draw: the focused session's
/// while [`render`] shows its cursor, else the user's own.
///
/// A [`Screen`] reports a steady block both for a program that never chose a cursor and for one that asked for a
/// steady block, so that is the user's own shape too: a session that never chose leaves the user's cursor alone.
pub fn cursor_style(client: &Client, screens: &Screens) -> SetCursorStyle {
    let Some(cursor) = cursor_session(client, screens).map(Screen::cursor) else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (cursor.shape, cursor.blinking) {
        (CursorShape::Block, false) => SetCursorStyle::DefaultUserShape,
        (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Beam, false) => SetCursorStyle::SteadyBar,
        (CursorShape::Beam, true) => SetCursorStyle::BlinkingBar,
    }
}

/// Where the session's own cursor is on the window, if it shows one inside the pane.
fn session_cursor(screen: &Screen, pane: Rect) -> Option<Position> {
    let cursor = screen.cursor();
    let at = Position::new(
        pane.x.checked_add(cursor.col)?,
        pane.y.checked_add(cursor.row)?,
    );
    (cursor.visible && pane.contains(at)).then_some(at)
}

/// The pane with no sessions: how to make one, or why there can't be one yet.
pub(crate) fn empty(client: &Client, pane: Rect, buf: &mut ratatui::buffer::Buffer) {
    let gray = Style::new().fg(Color::Gray);
    let lines = match client.connection() {
        Connection::Ready => vec![
            draw::line(shown("No sessions yet."), Style::new()),
            draw::line(shown(""), Style::new()),
            draw::line(shown("Click + New session to start one."), gray),
            draw::line(
                shown(&format!(
                    "Or press ctrl-{PREFIX} then n; ctrl-{PREFIX} then ? for help."
                )),
                gray,
            ),
        ],
        Connection::Failed(why) => vec![draw::line(shown(why), Style::new().fg(Color::Red))],
        Connection::Incompatible => vec![
            draw::line(
                shown("The shepherd host speaks another protocol version."),
                Style::new().fg(Color::Yellow),
            ),
            draw::line(shown("Click Restart host to restart it."), gray),
        ],
        _ => vec![draw::line(shown("Connecting to the shepherd host…"), gray)],
    };
    let height = (lines.len() as u16).min(pane.height);
    let area = Rect::new(
        pane.x,
        pane.y + (pane.height - height) / 2,
        pane.width,
        height,
    );
    draw::render(
        draw::paragraph(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
        buf,
    );
}
