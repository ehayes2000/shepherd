use std::path::Path;
use std::sync::Arc;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};
use vt100::Screen;

use crate::pty_widget::PtyWidget;

pub struct MainView;

impl MainView {
    pub fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        frame: &mut Frame,
        screen: Option<&Arc<Screen>>,
        active_name: Option<&str>,
        active_path: Option<&Path>,
        background_count: usize,
        stopped_count: usize,
        bottom_left: Line<'static>,
        bottom_center: Option<Line<'static>>,
        scroll_offset: usize,
    ) -> Rect {
        let area = frame.area();

        let top_title = match active_name {
            Some(name) => format!(" {} ", name),
            None => " No Session ".to_string(),
        };

        let total_sessions = background_count + if active_name.is_some() { 1 } else { 0 };
        let session_count_text = if total_sessions > 1 {
            format!("{} Sessions", total_sessions)
        } else {
            String::new()
        };

        let path_text = active_path.map(path_relative_to_home).unwrap_or_default();

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Line::from(top_title).left_aligned());

        // Bottom left: hotkeys
        block = block.title_bottom(bottom_left.left_aligned());

        // Bottom center: status message (if any)
        if let Some(center) = bottom_center {
            block = block.title_bottom(center.centered());
        }

        // Bottom right: stopped indicator + session count + path
        let mut right_spans: Vec<Span> = Vec::new();

        // Add stopped indicator if any sessions are stopped
        if stopped_count > 0 {
            right_spans.push(Span::styled(
                "●",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            right_spans.push(Span::styled(
                format!(" {}", stopped_count),
                Style::default().fg(Color::Yellow),
            ));
        }

        // Add separator if we have both indicator and other info
        if !right_spans.is_empty() && (!session_count_text.is_empty() || !path_text.is_empty()) {
            right_spans.push(Span::raw(" │ "));
        }

        // Add session count
        if !session_count_text.is_empty() {
            right_spans.push(Span::raw(session_count_text));
            if !path_text.is_empty() {
                right_spans.push(Span::raw(" │ "));
            }
        }

        // Add path
        if !path_text.is_empty() {
            right_spans.push(Span::raw(path_text));
        }

        // Add padding
        if !right_spans.is_empty() {
            right_spans.insert(0, Span::raw(" "));
            right_spans.push(Span::raw(" "));
        }

        if !right_spans.is_empty() {
            block = block.title_bottom(Line::from(right_spans).right_aligned());
        }

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(screen) = screen {
            let widget = PtyWidget::new(screen.as_ref()).scroll_offset(scroll_offset);
            frame.render_widget(widget, inner);
        }

        inner
    }
}

impl Default for MainView {
    fn default() -> Self {
        Self::new()
    }
}

fn path_relative_to_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}
