//! alacritty types → this crate's types.

use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::vte::ansi::{Color as AlacColor, CursorShape as AlacShape, NamedColor};

use crate::{
    Attrs, Cell, Color, CursorShape, Modes, MouseEncoding, MouseMode, MouseTracking, Underline,
    Width,
};

pub(crate) fn cell(cell: &AlacCell) -> Cell<'_> {
    let flags = cell.flags;
    let width = if flags.contains(Flags::WIDE_CHAR) {
        Width::Wide
    } else if flags.contains(Flags::WIDE_CHAR_SPACER) {
        Width::Spacer
    } else {
        Width::Single
    };
    Cell {
        ch: cell.c,
        zerowidth: cell.zerowidth().unwrap_or(&[]),
        fg: color(cell.fg),
        bg: color(cell.bg),
        underline_color: cell.underline_color().map(color),
        attrs: attrs(flags),
        width,
    }
}

pub(crate) fn color(color: AlacColor) -> Color {
    match color {
        AlacColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AlacColor::Indexed(index) => Color::Indexed(index),
        AlacColor::Named(named) => named_color(named),
    }
}

fn named_color(named: NamedColor) -> Color {
    let index = named as usize;
    if index < 16 {
        return Color::Ansi(index as u8);
    }
    // Dim variants only appear when alacritty renders; map them back to their base colour just in case.
    match named {
        NamedColor::DimBlack => Color::Ansi(0),
        NamedColor::DimRed => Color::Ansi(1),
        NamedColor::DimGreen => Color::Ansi(2),
        NamedColor::DimYellow => Color::Ansi(3),
        NamedColor::DimBlue => Color::Ansi(4),
        NamedColor::DimMagenta => Color::Ansi(5),
        NamedColor::DimCyan => Color::Ansi(6),
        NamedColor::DimWhite => Color::Ansi(7),
        _ => Color::Default,
    }
}

pub(crate) fn attrs(flags: Flags) -> Attrs {
    let underline = if flags.contains(Flags::UNDERCURL) {
        Underline::Curly
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        Underline::Single
    } else {
        Underline::None
    };
    Attrs {
        bold: flags.contains(Flags::BOLD),
        dim: flags.contains(Flags::DIM),
        italic: flags.contains(Flags::ITALIC),
        underline,
        inverse: flags.contains(Flags::INVERSE),
        hidden: flags.contains(Flags::HIDDEN),
        strikeout: flags.contains(Flags::STRIKEOUT),
    }
}

pub(crate) fn cursor_shape(shape: AlacShape) -> CursorShape {
    match shape {
        AlacShape::Underline => CursorShape::Underline,
        AlacShape::Beam => CursorShape::Beam,
        AlacShape::Block | AlacShape::HollowBlock | AlacShape::Hidden => CursorShape::Block,
    }
}

pub(crate) fn modes(mode: TermMode) -> Modes {
    let tracking = if mode.contains(TermMode::MOUSE_MOTION) {
        MouseTracking::Motion
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        MouseTracking::Drag
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        MouseTracking::Click
    } else {
        MouseTracking::Off
    };
    let encoding = if mode.contains(TermMode::SGR_MOUSE) {
        MouseEncoding::Sgr
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        MouseEncoding::Utf8
    } else {
        MouseEncoding::X10
    };
    Modes {
        kitty_keyboard: kitty_flags(mode),
        mouse: MouseMode { tracking, encoding },
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        focus: mode.contains(TermMode::FOCUS_IN_OUT),
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        app_cursor: mode.contains(TermMode::APP_CURSOR),
    }
}

/// The kitty protocol's own flag numbering, from alacritty's mode bits.
pub(crate) fn kitty_flags(mode: TermMode) -> u8 {
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .into_iter()
    .filter(|(bit, _)| mode.contains(*bit))
    .map(|(_, flag)| flag)
    .sum()
}
