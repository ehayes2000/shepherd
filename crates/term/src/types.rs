//! Emulator-neutral views of the screen. Nothing here exposes `alacritty_terminal` types.

use std::time::Duration;

/// A cell colour as the program set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// The terminal's default foreground or background (SGR 39 / 49).
    Default,
    /// One of the 16 basic colours set with SGR 30-37 / 90-97 (and the 40s / 100s): 0-7 normal, 8-15 bright.
    Ansi(u8),
    /// A 256-colour palette index set with `38;5;n` / `48;5;n`.
    Indexed(u8),
    /// A 24-bit colour.
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Text attributes of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attrs {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: Underline,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

/// How many columns a cell's character occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Width {
    /// A normal, one-column character.
    Single,
    /// The first column of a two-column character.
    Wide,
    /// The second column of a two-column character. It has no character of its own; skip it when drawing.
    Spacer,
}

/// A read-only view of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell<'a> {
    /// The base character. A blank cell is `' '`.
    pub ch: char,
    /// Combining characters drawn on top of `ch`.
    pub zerowidth: &'a [char],
    pub fg: Color,
    pub bg: Color,
    /// `None` means the underline uses the foreground colour.
    pub underline_color: Option<Color>,
    pub attrs: Attrs,
    pub width: Width,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

/// The real terminal cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blinking: bool,
}

/// Which mouse events the program asked for (DECSET 1000 / 1002 / 1003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MouseTracking {
    #[default]
    Off,
    /// Presses and releases.
    Click,
    /// Presses, releases, and motion while a button is held.
    Drag,
    /// Every motion.
    Motion,
}

/// How mouse reports are encoded (DECSET 1005 / 1006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MouseEncoding {
    /// The legacy `CSI M Cb Cx Cy` byte encoding.
    #[default]
    X10,
    Utf8,
    Sgr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MouseMode {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
}

/// Input modes the program has turned on. Input encoding follows these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modes {
    /// Active kitty keyboard protocol flags, as the program set them (`1` disambiguate, `2` event types,
    /// `4` alternate keys, `8` all keys as escapes, `16` associated text). `0` means legacy encoding.
    pub kitty_keyboard: u8,
    pub mouse: MouseMode,
    pub bracketed_paste: bool,
    /// Focus in/out reports (DECSET 1004).
    pub focus: bool,
    pub alt_screen: bool,
    /// Application cursor keys (DECCKM): arrows send `ESC O A` instead of `ESC [ A`.
    pub app_cursor: bool,
}

/// Output held by an open synchronized update (`CSI ? 2026 h`).
///
/// The screen reads no clock. The caller starts a timer for `timeout` when it sees a new `id`, and calls
/// [`crate::Screen::expire_sync`] with that `id` when the timer fires. Every new begin marker gives a new `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncHold {
    pub id: u64,
    pub timeout: Duration,
}
