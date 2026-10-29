use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use vt100::Screen;

/// A widget that renders a vt100 terminal screen
pub struct PtyWidget<'a> {
    screen: &'a Screen,
    scroll_offset: usize,
}

impl<'a> PtyWidget<'a> {
    pub fn new(screen: &'a Screen) -> Self {
        Self {
            screen,
            scroll_offset: 0,
        }
    }

    pub fn scroll_offset(mut self, offset: usize) -> Self {
        self.scroll_offset = offset;
        self
    }
}

impl Widget for PtyWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (screen_rows, screen_cols) = self.screen.size();
        let display_rows = area.height.min(screen_rows);
        let cols = area.width.min(screen_cols);

        if self.scroll_offset == 0 {
            // No scrollback - render current screen directly
            self.render_screen(self.screen, area, buf, display_rows, cols);
        } else {
            // Scrollback mode - clone screen and set scrollback position
            // vt100's set_scrollback(n) shows the view starting n rows from the top of scrollback
            // We want scroll_offset to mean "how many lines up from bottom"
            // So we need to convert: scrollback_position = scroll_offset
            let mut scrolled_screen = self.screen.clone();
            scrolled_screen.set_scrollback(self.scroll_offset);
            self.render_screen(&scrolled_screen, area, buf, display_rows, cols);
        }
    }
}

impl PtyWidget<'_> {
    fn render_screen(
        &self,
        screen: &Screen,
        area: Rect,
        buf: &mut Buffer,
        display_rows: u16,
        cols: u16,
    ) {
        for row in 0..display_rows {
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    let style = vt100_to_ratatui_style(cell);
                    let x = area.x + col;
                    let y = area.y + row;

                    if x < buf.area.width && y < buf.area.height {
                        let contents = cell.contents();
                        if contents.is_empty() {
                            buf[(x, y)].set_char(' ').set_style(style);
                        } else {
                            buf.set_string(x, y, contents, style);
                        }
                    }
                }
            }
        }
    }
}

fn vt100_to_ratatui_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();

    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    style = style.bg(vt100_color_to_ratatui(cell.bgcolor()));

    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    style
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
