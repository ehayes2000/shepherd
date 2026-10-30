//! Pure rendering of client state and key mapping. See `docs/architecture.md`.
//!
//! - [`render`] draws the sidebar ([`sidebar`]), the selected session's terminal in the pane ([`layout`]), and any
//!   popup.
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
//! | unknown                          | `?`   | dark gray  |
//!
//! A session that wants the user (blocked on them, or idle since they last looked) also gets a yellow `▌` on the
//! left edge of its row, in both the full and the collapsed sidebar.

pub mod encode;
mod help;
mod keys;
pub mod layout;
mod screen;
pub mod sidebar;

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use shepherd_core::client::{Client, Connection, Focus};
use shepherd_core::ids::SessionId;
use shepherd_term::Screen;

pub use help::BINDINGS;
pub use keys::{PREFIX, map_event, map_key};
pub use layout::{Layout, layout, pane_size};
pub use screen::render_screen;

/// Each session's terminal, as the client runtime keeps them.
pub type Screens = BTreeMap<SessionId, Screen>;

/// Draws the whole window.
pub fn render(client: &Client, screens: &Screens, frame: &mut Frame) {
    let layout = layout(frame.area());
    let selected = client.selected().and_then(|id| screens.get(id));
    let buf = frame.buffer_mut();
    sidebar::render(client, &layout, buf);
    match selected {
        Some(screen) => render_screen(screen, layout.pane, buf),
        None if client.sessions().is_empty() => empty(client, layout.pane, buf),
        None => {}
    }
    let popup_cursor = help::render(client, frame.area(), frame.buffer_mut());
    let cursor = match (client.popup(), selected) {
        (Some(_), _) => popup_cursor,
        (None, Some(screen)) if client.focus() == Focus::Terminal => {
            session_cursor(screen, layout.pane)
        }
        (None, _) => None,
    };
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
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
fn empty(client: &Client, pane: Rect, buf: &mut ratatui::buffer::Buffer) {
    let gray = Style::new().fg(Color::Gray);
    let lines = match client.connection() {
        Connection::Ready => vec![
            Line::raw("No sessions yet."),
            Line::raw(""),
            Line::styled(format!("Press ctrl-{PREFIX} then n to start one,"), gray),
            Line::styled(format!("or ctrl-{PREFIX} then ? for help."), gray),
        ],
        Connection::Failed(why) => vec![Line::styled(why.clone(), Style::new().fg(Color::Red))],
        _ => vec![Line::styled("Connecting to the shepherd host…", gray)],
    };
    let height = (lines.len() as u16).min(pane.height);
    let area = Rect::new(
        pane.x,
        pane.y + (pane.height - height) / 2,
        pane.width,
        height,
    );
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}
