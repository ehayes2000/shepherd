//! Draws a terminal [`Screen`] into a ratatui [`Buffer`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use shepherd_term::{self as term, Cell, Screen, Underline, Width};

/// Draws the screen's cells into `area`, starting at its top-left corner and clipped to it. Cells of `buf` outside
/// the screen's size are left alone. The cursor is not drawn; place it with the frame's cursor position.
pub fn render_screen(screen: &Screen, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    let (cols, rows) = screen.size();
    let width = cols.min(area.width);
    for row in 0..rows.min(area.height) {
        let y = area.y + row;
        for (col, cell) in screen.row(row).take(usize::from(width)).enumerate() {
            let x = area.x + col as u16;
            let style = style(&cell);
            match cell.width {
                // Drawn along with the wide char before it.
                Width::Spacer => {}
                // Half of it would be outside the area.
                Width::Wide if col as u16 + 1 >= width => {
                    buf[(x, y)].reset();
                    buf[(x, y)].set_style(style);
                }
                Width::Single | Width::Wide => {
                    let target = &mut buf[(x, y)];
                    target.set_symbol(&symbol(&cell)).set_style(style);
                    if cell.width == Width::Wide {
                        // Hidden behind the wide char, as `Buffer::set_string` leaves it, but keeping the style
                        // so a highlighted row stays one style run.
                        buf[(x + 1, y)].reset();
                        buf[(x + 1, y)].set_style(style);
                    }
                }
            }
        }
    }
}

fn symbol(cell: &Cell<'_>) -> String {
    let base = if cell.ch.is_control() { ' ' } else { cell.ch };
    std::iter::once(base)
        .chain(cell.zerowidth.iter().copied())
        .collect()
}

fn style(cell: &Cell<'_>) -> Style {
    let attrs = cell.attrs;
    let mut modifier = Modifier::empty();
    for (on, flag) in [
        (attrs.bold, Modifier::BOLD),
        (attrs.dim, Modifier::DIM),
        (attrs.italic, Modifier::ITALIC),
        (attrs.underline != Underline::None, Modifier::UNDERLINED),
        (attrs.inverse, Modifier::REVERSED),
        (attrs.hidden, Modifier::HIDDEN),
        (attrs.strikeout, Modifier::CROSSED_OUT),
    ] {
        if on {
            modifier |= flag;
        }
    }
    Style::new()
        .fg(color(cell.fg))
        .bg(color(cell.bg))
        .add_modifier(modifier)
}

fn color(color: term::Color) -> Color {
    const ANSI: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    match color {
        term::Color::Default => Color::Reset,
        term::Color::Ansi(n) => ANSI
            .get(usize::from(n))
            .copied()
            .unwrap_or(Color::Indexed(n)),
        term::Color::Indexed(n) => Color::Indexed(n),
        term::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
