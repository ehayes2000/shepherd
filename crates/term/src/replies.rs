//! Turns the emulator's events into bytes for the PTY.

use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{NamedColor, Rgb};

/// Collects the `Term`'s events. Cloning shares the queue, so `Screen` can drain what the `Term` sent.
#[derive(Clone, Default)]
pub(crate) struct Listener(Arc<Mutex<Vec<Event>>>);

impl Listener {
    pub(crate) fn drain(&self) -> Vec<Event> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

/// Default foreground and background to report when the program hasn't set its own.
pub(crate) struct ReportedColors {
    pub(crate) fg: (u8, u8, u8),
    pub(crate) bg: (u8, u8, u8),
}

impl Default for ReportedColors {
    fn default() -> Self {
        Self {
            fg: (0xff, 0xff, 0xff),
            bg: (0, 0, 0),
        }
    }
}

/// The reply bytes for `events`, in order. Colour queries report colours the program set, else `reported` for
/// the default foreground and background, else xterm's palette. Events that aren't replies (title, bell,
/// clipboard, ...) are dropped.
pub(crate) fn answer(events: Vec<Event>, colors: &Colors, reported: &ReportedColors) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        match event {
            Event::PtyWrite(text) => out.extend_from_slice(text.as_bytes()),
            Event::ColorRequest(index, format) => {
                let color = colors[index].unwrap_or_else(|| default_color(index, reported));
                out.extend_from_slice(format(color).as_bytes());
            }
            _ => {}
        }
    }
    out
}

/// `reported` for the default colours, and xterm's default 256-colour palette.
fn default_color(index: usize, reported: &ReportedColors) -> Rgb {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    let rgb = |(r, g, b)| Rgb { r, g, b };
    let level = |n: usize| if n == 0 { 0 } else { 55 + 40 * n as u8 };
    match index {
        0..16 => rgb(ANSI[index]),
        16..232 => {
            let n = index - 16;
            rgb((level(n / 36), level(n / 6 % 6), level(n % 6)))
        }
        232..256 => {
            let gray = 8 + 10 * (index - 232) as u8;
            rgb((gray, gray, gray))
        }
        _ if index == NamedColor::Background as usize => rgb(reported.bg),
        _ => rgb(reported.fg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_palette_matches_xterm() {
        let rgb = |c: Rgb| (c.r, c.g, c.b);
        let d = ReportedColors::default();
        assert_eq!(rgb(default_color(1, &d)), (0xcd, 0, 0));
        assert_eq!(rgb(default_color(16, &d)), (0, 0, 0));
        assert_eq!(rgb(default_color(196, &d)), (0xff, 0, 0));
        assert_eq!(rgb(default_color(231, &d)), (0xff, 0xff, 0xff));
        assert_eq!(rgb(default_color(232, &d)), (8, 8, 8));
        assert_eq!(rgb(default_color(255, &d)), (0xee, 0xee, 0xee));
        assert_eq!(
            rgb(default_color(NamedColor::Foreground as usize, &d)),
            (0xff, 0xff, 0xff)
        );
        assert_eq!(
            rgb(default_color(NamedColor::Background as usize, &d)),
            (0, 0, 0)
        );
    }
}
