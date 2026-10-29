use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub struct HelpPopup;

impl HelpPopup {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let hotkeys = [
            ("ctrl+h", "Help"),
            ("ctrl+n", "New session"),
            ("ctrl+l", "List sessions"),
            ("ctrl+k", "Cleanup worktrees"),
            ("ctrl+x", "Kill session"),
            ("ctrl+d", "Quit"),
        ];

        let content_width = hotkeys
            .iter()
            .map(|(k, v)| k.len() + 3 + v.len())
            .max()
            .unwrap_or(20);

        let popup_width = (content_width as u16 + 4).min(area.width.saturating_sub(4));
        let popup_height = (hotkeys.len() as u16 + 2).min(area.height.saturating_sub(2));

        let popup_x = (area.width.saturating_sub(popup_width)) / 2;
        let popup_y = (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

        frame.render_widget(Clear, popup_area);

        let lines: Vec<Line> = hotkeys
            .iter()
            .map(|(key, desc)| {
                Line::from(vec![
                    Span::styled(
                        *key,
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" - "),
                    Span::raw(*desc),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White))
                .style(Style::default().bg(Color::Black)),
        );

        frame.render_widget(paragraph, popup_area);
    }
}

impl Default for HelpPopup {
    fn default() -> Self {
        Self::new()
    }
}
