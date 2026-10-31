//! What color a machine's name is drawn in (T62): [`style`] is the one helper every place that draws a machine's name
//! uses (the sidebar's machine headers and line 2, the machine selector in the new-session popup and the switcher), so
//! its sessions are recognizable everywhere. The color itself is core's ([`Client::machine_color`]); [`color`] maps the
//! palette to terminal colors, none of them the default background or the selection's.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use shepherd_core::client::Client;
use shepherd_core::ids::MachineId;
use shepherd_core::machine::MachineColor;

use crate::width::columns;

/// The terminal color `color` is drawn in.
pub fn color(color: MachineColor) -> Color {
    match color {
        MachineColor::Cyan => Color::Cyan,
        MachineColor::Magenta => Color::Magenta,
        MachineColor::Blue => Color::Blue,
        MachineColor::LightCyan => Color::LightCyan,
        MachineColor::LightMagenta => Color::LightMagenta,
        MachineColor::LightBlue => Color::LightBlue,
    }
}

/// How `machine`'s name is drawn: in its color. Patched over the style of what it is drawn with, so modifiers (bold,
/// dim) stay.
pub(crate) fn style(client: &Client, machine: &MachineId) -> Style {
    Style::new().fg(color(client.machine_color(machine)))
}

/// Colors `machine`'s name, `name` as drawn at `x`, `y`, in at most `width` columns: for a name drawn as part of a
/// longer text, or cut short.
pub(crate) fn tint(
    client: &Client,
    machine: &MachineId,
    name: &str,
    (x, y): (u16, u16),
    width: usize,
    buf: &mut Buffer,
) {
    let cols = u16::try_from(columns(name).min(width)).unwrap_or(u16::MAX);
    let area = Rect::new(x, y, cols, 1).intersection(buf.area);
    buf.set_style(area, style(client, machine));
}
