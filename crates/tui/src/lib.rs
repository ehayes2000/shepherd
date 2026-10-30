//! Pure rendering of client state and key mapping. See `docs/architecture.md`.
//!
//! - [`render`] draws the sidebar ([`sidebar`]), the selected session's terminal in the pane ([`layout`]), and any
//!   popup or menu. [`cursor_style`] is the cursor shape to go with it, which a ratatui frame can't carry.
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
//! A session that wants the user (blocked on them, or idle since they last looked) also gets a yellow `▌` on the
//! left edge of its row, in both the full and the collapsed sidebar.

mod draw;
pub mod encode;
mod keys;
pub mod layout;
mod popup;
mod screen;
pub mod sidebar;
mod width;

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Widget, Wrap};
use shepherd_core::client::{Client, Connection, Focus};
use shepherd_core::ids::SessionId;
use shepherd_term::{CursorShape, Screen};

use draw::shown;
pub use keys::{PREFIX, Pointer, from_legacy_terminal, map_event, map_key};
pub use layout::{Layout, Target, hit, layout, pane_size};
pub use popup::BINDINGS;
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
    let popup_cursor = popup::render(client, frame.area(), frame.buffer_mut());
    let cursor = match (client.popup(), cursor_session(client, screens)) {
        (Some(_), _) => popup_cursor,
        (None, Some(screen)) => session_cursor(screen, layout.pane),
        (None, None) => None,
    };
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

/// The session whose cursor [`render`] shows: the selected one, while keys go to it and no popup is open.
fn cursor_session<'a>(client: &Client, screens: &'a Screens) -> Option<&'a Screen> {
    if client.popup().is_some() || client.focus() != Focus::Terminal {
        return None;
    }
    screens.get(client.selected()?)
}

/// The shape and blink of the real terminal's cursor, for the runtime to set after each draw: the selected session's
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
fn empty(client: &Client, pane: Rect, buf: &mut ratatui::buffer::Buffer) {
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
        _ => vec![draw::line(shown("Connecting to the shepherd host…"), gray)],
    };
    let height = (lines.len() as u16).min(pane.height);
    let area = Rect::new(
        pane.x,
        pane.y + (pane.height - height) / 2,
        pane.width,
        height,
    );
    draw::paragraph(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}
