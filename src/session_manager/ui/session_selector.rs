use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::session_manager::session_pair::SessionActivity;

/// Categories of items in the session selector
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorItemKind {
    /// A live session (can be previewed/switched to)
    Live,
    /// A dead session (should be resumed)
    Dead,
}

/// A filterable session selector with incremental search.
pub struct SessionSelector {
    /// The current filter query
    query: String,
    /// Selection state for the filtered list
    state: ListState,
    /// Indices of sessions that match the current filter
    filtered_indices: Vec<usize>,
    /// Index of the active session (highlighted green)
    active_index: Option<usize>,
    /// Number of live sessions
    live_count: usize,
    /// Number of dead sessions (after live)
    dead_count: usize,
}

impl SessionSelector {
    pub fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            query: String::new(),
            state,
            filtered_indices: Vec::new(),
            active_index: None,
            live_count: 0,
            dead_count: 0,
        }
    }

    /// Reset the selector state for a fresh selection.
    pub fn reset(&mut self) {
        self.query.clear();
        self.filtered_indices.clear();
        self.state.select(Some(0));
        self.live_count = 0;
        self.dead_count = 0;
    }

    /// Set the index of the active session (will be highlighted green).
    pub fn set_active_index(&mut self, index: Option<usize>) {
        self.active_index = index;
    }

    /// Set the counts for different item categories.
    pub fn set_counts(&mut self, live_count: usize, dead_count: usize) {
        self.live_count = live_count;
        self.dead_count = dead_count;
    }

    /// Add a character to the query and update the filter.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Remove the last character from the query.
    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// Get the currently selected index in the original session list.
    /// Returns None if no sessions match the filter.
    pub fn selected_original_index(&self) -> Option<usize> {
        let selected = self.state.selected()?;
        self.filtered_indices.get(selected).copied()
    }

    /// Get the kind of the currently selected item.
    pub fn selected_kind(&self) -> Option<SelectorItemKind> {
        let idx = self.selected_original_index()?;
        Some(self.item_kind(idx))
    }

    /// Determine the kind of item at a given index.
    fn item_kind(&self, idx: usize) -> SelectorItemKind {
        if idx < self.live_count {
            SelectorItemKind::Live
        } else {
            SelectorItemKind::Dead
        }
    }

    /// Move selection up in the filtered list.
    pub fn move_up(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let next = if current == 0 {
            self.filtered_indices.len() - 1
        } else {
            current - 1
        };
        self.state.select(Some(next));
    }

    /// Move selection down in the filtered list.
    pub fn move_down(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let next = if current >= self.filtered_indices.len() - 1 {
            0
        } else {
            current + 1
        };
        self.state.select(Some(next));
    }

    /// Update the filtered indices based on the current query.
    /// Call this after modifying the query or when the session list changes.
    pub fn update_filter(&mut self, sessions: &[(String, String)]) {
        let query_lower = self.query.to_lowercase();

        self.filtered_indices = sessions
            .iter()
            .enumerate()
            .filter(|(_, (name, path))| {
                if query_lower.is_empty() {
                    true
                } else {
                    name.to_lowercase().contains(&query_lower)
                        || path.to_lowercase().contains(&query_lower)
                }
            })
            .map(|(i, _)| i)
            .collect();

        // Ensure selection stays valid
        if self.filtered_indices.is_empty() {
            self.state.select(None);
        } else {
            let current = self.state.selected().unwrap_or(0);
            if current >= self.filtered_indices.len() {
                self.state.select(Some(self.filtered_indices.len() - 1));
            }
        }
    }

    /// Render the session selector.
    /// `sessions` is a slice of (name, path) tuples.
    /// `session_states` maps session names to their current activity state.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        sessions: &[(String, String)],
        session_states: &HashMap<String, SessionActivity>,
    ) {
        // Calculate popup dimensions
        let max_name_len = sessions
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);
        let max_path_len = sessions
            .iter()
            .map(|(_, path)| path.len())
            .max()
            .unwrap_or(10);

        // Width: name + separator + path + padding + borders (or just path for worktrees)
        let content_width = if max_name_len > 0 {
            max_name_len + 3 + max_path_len + 4
        } else {
            max_path_len + 4
        };
        let popup_width = content_width.max(30).min(area.width as usize - 4) as u16;

        // Height: input box (3) + list items + borders
        let max_visible = 10usize;
        let list_height = self.filtered_indices.len().min(max_visible).max(1) as u16;
        let popup_height = (3 + list_height + 2).min(area.height - 2);

        // Center the popup
        let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_height);

        // Clear the popup area
        frame.render_widget(Clear, popup_area);

        // Split popup into input area and list area
        let input_area = Rect::new(popup_area.x, popup_area.y, popup_area.width, 3);
        let list_area = Rect::new(
            popup_area.x,
            popup_area.y + 3,
            popup_area.width,
            popup_area.height - 3,
        );

        // Render input box
        let input_text = format!("{}_", self.query);
        let input = Paragraph::new(input_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::White))
                    .title(" Filter "),
            )
            .style(Style::default().fg(Color::White));
        frame.render_widget(input, input_area);

        // Build filtered list items
        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .map(|&i| {
                let (name, path) = &sessions[i];
                let is_active = self.active_index == Some(i);
                let kind = self.item_kind(i);
                let activity = session_states.get(name);
                let available_width = (popup_width as usize).saturating_sub(4);

                // Account for status indicator in width calculation (2 chars: "● ")
                // Live sessions always have an indicator (purple=running, yellow=stopped)
                let has_indicator = kind == SelectorItemKind::Live;
                let indicator_width = if has_indicator { 2 } else { 0 };
                let path_width = available_width
                    .saturating_sub(name.len() + 3)
                    .saturating_sub(indicator_width);

                let path_display = if path.len() > path_width {
                    format!("...{}", &path[path.len().saturating_sub(path_width - 3)..])
                } else {
                    path.clone()
                };

                let padding = available_width
                    .saturating_sub(name.len())
                    .saturating_sub(path_display.len())
                    .saturating_sub(indicator_width);

                // Active session: green, dead: dark gray, normal live: white
                let name_style = if is_active {
                    Style::default().fg(Color::Green)
                } else if kind == SelectorItemKind::Dead {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::White)
                };

                let path_style = if kind == SelectorItemKind::Dead {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Gray)
                };

                // Build spans with status indicator for live sessions
                let mut spans = Vec::new();
                if has_indicator {
                    let indicator_color = match activity {
                        Some(SessionActivity::Stopped) => Color::Yellow, // Needs attention
                        Some(SessionActivity::RunningTool(_)) => Color::Cyan, // Running a tool
                        _ => Color::Magenta,                             // Active/default
                    };
                    spans.push(Span::styled("● ", Style::default().fg(indicator_color)));
                }
                spans.push(Span::styled(name.clone(), name_style));
                spans.push(Span::raw(" ".repeat(padding)));
                spans.push(Span::styled(path_display, path_style));

                Line::from(spans)
            })
            .map(ListItem::new)
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::White)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, list_area, &mut self.state);
    }
}

impl Default for SessionSelector {
    fn default() -> Self {
        Self::new()
    }
}
