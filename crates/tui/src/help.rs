//! Popups drawn over everything: help, the rename input, and the close confirmation.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use shepherd_core::client::{Client, Popup};

/// The prefix bindings, as the help lists them.
pub const BINDINGS: [(&str, &str); 8] = [
    ("ctrl-b n", "new session"),
    ("ctrl-b j / ↓", "next session"),
    ("ctrl-b k / ↑", "previous session"),
    ("ctrl-b r", "rename session"),
    ("ctrl-b x", "close session"),
    ("ctrl-b ?", "this help"),
    ("ctrl-b q", "detach (agents keep running)"),
    ("ctrl-b ctrl-b", "send ctrl-b to the session"),
];

/// Draws the open popup, if any, centered on `area`. Returns where the cursor goes, for a popup with an input.
pub(crate) fn render(client: &Client, area: Rect, buf: &mut Buffer) -> Option<Position> {
    match client.popup()? {
        Popup::Help => {
            help(area, buf);
            None
        }
        Popup::Rename { name, .. } => rename(name, area, buf),
        Popup::ConfirmClose { session } => {
            let name = client
                .sessions()
                .iter()
                .find(|s| &s.id == session)
                .map_or(session.as_str(), |s| s.name.as_str());
            confirm_close(name, area, buf);
            None
        }
    }
}

fn help(area: Rect, buf: &mut Buffer) {
    let key = Style::new().fg(Color::Cyan);
    let mut lines: Vec<Line> = BINDINGS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:<14}"), key),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Other keys go to the session. Any key closes this.",
        Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
    ));
    let height = lines.len() as u16 + 2;
    let box_area = centered(area, 56, height);
    popup(box_area, " help ", buf);
    Paragraph::new(lines).render(inner(box_area), buf);
}

fn rename(name: &str, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let box_area = centered(area, 40, 4);
    popup(box_area, " rename session ", buf);
    let inside = inner(box_area);
    if inside.width < 2 || inside.height < 2 {
        return None;
    }
    // Keep the end of a long name in view, with a column for the cursor.
    let room = usize::from(inside.width - 1);
    let chars: Vec<char> = name.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    let (end, _) = buf.set_stringn(inside.x, inside.y, &shown, room, Style::new());
    let hint = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    buf.set_stringn(
        inside.x,
        inside.y + 1,
        "enter to rename · esc to cancel",
        usize::from(inside.width),
        hint,
    );
    Some(Position::new(end, inside.y))
}

fn confirm_close(name: &str, area: Rect, buf: &mut Buffer) {
    let box_area = centered(area, 40, 4);
    popup(box_area, " close session ", buf);
    let lines = vec![
        Line::from(vec![
            Span::raw("Close "),
            Span::styled(name, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("? Its agent is stopped."),
        ]),
        Line::styled(
            "y / enter to close · n / esc to keep",
            Style::new().fg(Color::Gray),
        ),
    ];
    Paragraph::new(lines).render(inner(box_area), buf);
}

/// Clears `area` and draws a bordered box titled `title` on it.
fn popup(area: Rect, title: &str, buf: &mut Buffer) {
    Clear.render(area, buf);
    Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::Cyan))
        .render(area, buf);
}

fn inner(area: Rect) -> Rect {
    Block::bordered().inner(area)
}

/// A `width` x `height` rect in the middle of `area`, shrunk to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
