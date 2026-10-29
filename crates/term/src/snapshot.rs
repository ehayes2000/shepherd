//! Serializes a screen as escape sequences that redraw it in a fresh (or reset) screen of the same size.

use std::io::Write;

use alacritty_terminal::grid::{Cursor, Dimensions};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor};

use crate::convert::kitty_flags;

/// Cell flags that SGR sets. The rest (wide-char bookkeeping, wrap) come from how the cells are written.
const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// The SGR state of a cell or of the cursor's template.
#[derive(Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    underline: Option<Color>,
    flags: Flags,
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            underline: cell.underline_color(),
            flags: cell.flags & STYLE,
        }
    }
}

pub(crate) fn ansi<T>(term: &Term<T>) -> Vec<u8> {
    let mut out = Writer {
        buf: Vec::new(),
        pen: Pen::of(&Cell::default()),
    };
    let mode = *term.mode();

    // RIS, so the target starts from the same blank state whether it is fresh or not.
    out.raw("\x1bc");
    if mode.contains(TermMode::ALT_SCREEN) {
        out.raw("\x1b[?1049h");
    }
    cells(&mut out, term);
    // Origin mode homes the cursor, so it goes before the cursor is placed. The scroll region is the whole screen
    // after RIS, so absolute and origin-relative positions agree.
    if mode.contains(TermMode::ORIGIN) {
        out.raw("\x1b[?6h");
    }
    saved_cursor(&mut out, term);
    cursor(&mut out, term);
    out.pen(&Pen::of(&term.grid().cursor.template));
    modes(&mut out, term);
    out.buf
}

struct Writer {
    buf: Vec<u8>,
    /// The pen the target is drawing with.
    pen: Pen,
}

impl Writer {
    fn raw(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    fn fmt(&mut self, args: std::fmt::Arguments<'_>) {
        // Writing into a Vec cannot fail.
        let _ = self.buf.write_fmt(args);
    }

    fn goto(&mut self, row: usize, col: usize) {
        self.fmt(format_args!("\x1b[{};{}H", row + 1, col + 1));
    }

    fn pen(&mut self, pen: &Pen) {
        if *pen == self.pen {
            return;
        }
        self.raw("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, ";1"),
            (Flags::DIM, ";2"),
            (Flags::ITALIC, ";3"),
            (Flags::UNDERLINE, ";4"),
            (Flags::DOUBLE_UNDERLINE, ";4:2"),
            (Flags::UNDERCURL, ";4:3"),
            (Flags::DOTTED_UNDERLINE, ";4:4"),
            (Flags::DASHED_UNDERLINE, ";4:5"),
            (Flags::INVERSE, ";7"),
            (Flags::HIDDEN, ";8"),
            (Flags::STRIKEOUT, ";9"),
        ] {
            if pen.flags.contains(flag) {
                self.raw(code);
            }
        }
        self.color(pen.fg, 30, 90, 38);
        self.color(pen.bg, 40, 100, 48);
        match pen.underline {
            Some(Color::Spec(c)) => self.fmt(format_args!(";58;2;{};{};{}", c.r, c.g, c.b)),
            Some(Color::Indexed(i)) => self.fmt(format_args!(";58;5;{i}")),
            Some(Color::Named(n)) if (n as usize) < 16 => {
                self.fmt(format_args!(";58;5;{}", n as usize))
            }
            Some(Color::Named(_)) | None => {}
        }
        self.raw("m");
        self.pen = pen.clone();
    }

    fn color(&mut self, color: Color, normal: usize, bright: usize, extended: usize) {
        match color {
            Color::Spec(c) => self.fmt(format_args!(";{extended};2;{};{};{}", c.r, c.g, c.b)),
            Color::Indexed(i) => self.fmt(format_args!(";{extended};5;{i}")),
            Color::Named(n) => {
                let n = base_color(n);
                match n {
                    0..8 => self.fmt(format_args!(";{}", normal + n)),
                    8..16 => self.fmt(format_args!(";{}", bright + n - 8)),
                    _ => {}
                }
            }
        }
    }

    /// Writes a cell's character at the target's cursor.
    fn put(&mut self, cell: &Cell) {
        self.pen(&Pen::of(cell));
        let mut utf8 = [0; 4];
        self.raw(cell.c.encode_utf8(&mut utf8));
        for c in cell.zerowidth().unwrap_or(&[]) {
            self.raw(c.encode_utf8(&mut utf8));
        }
    }
}

/// The palette index of a named colour, folding the render-only dim variants into their base colour.
fn base_color(named: NamedColor) -> usize {
    let index = named as usize;
    let dim_black = NamedColor::DimBlack as usize;
    if (dim_black..dim_black + 8).contains(&index) {
        index - dim_black
    } else {
        index
    }
}

fn cells<T>(out: &mut Writer, term: &Term<T>) {
    let grid = term.grid();
    let cols = grid.columns();
    let blank = Cell::default();
    let mut continues = false;
    for row in 0..grid.screen_lines() {
        let line = &grid[Line(row as i32)];
        let wraps = line[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let mut end = if wraps {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&col| line[Column(col)] != blank)
                .map_or(0, |col| col + 1)
        };
        if continues {
            // The target's cursor is waiting to wrap from the row above; printing one cell makes it wrap.
            end = end.max(1);
        } else if end > 0 {
            out.goto(row, 0);
        }
        for col in 0..end {
            let cell = &line[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Written together with the wide char before it.
                continue;
            }
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                // Re-created when the wide char starting the next row doesn't fit here.
                continue;
            }
            if cell.c == '\t' {
                // A tab moves the cursor instead of printing; draw the cell's style first, then mark it as a tab.
                out.put(&Cell {
                    c: ' ',
                    ..cell.clone()
                });
                out.goto(row, col);
                out.raw("\t");
                out.goto(row, col + 1);
                continue;
            }
            out.put(cell);
        }
        continues = wraps;
    }
}

fn saved_cursor<T>(out: &mut Writer, term: &Term<T>) {
    let saved = &term.grid().saved_cursor;
    let (row, col) = clamped(term, saved);
    out.goto(row, col);
    out.pen(&Pen::of(&saved.template));
    out.raw("\x1b7");
}

fn cursor<T>(out: &mut Writer, term: &Term<T>) {
    let grid = term.grid();
    let (row, col) = clamped(term, &grid.cursor);
    if !grid.cursor.input_needs_wrap {
        out.goto(row, col);
        return;
    }
    // The cursor sits past the last column. Re-printing the last character puts the target in the same state.
    let line = &grid[Line(row as i32)];
    let start = if col > 0 && line[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
        col - 1
    } else {
        col
    };
    out.goto(row, start);
    out.put(&line[Column(start)]);
}

fn clamped<T, C>(term: &Term<T>, cursor: &Cursor<C>) -> (usize, usize) {
    let row = usize::try_from(cursor.point.line.0)
        .unwrap_or(0)
        .min(term.screen_lines() - 1);
    let col = cursor.point.column.0.min(term.columns() - 1);
    (row, col)
}

fn modes<T>(out: &mut Writer, term: &Term<T>) {
    let mode = *term.mode();
    let kitty = kitty_flags(mode);
    if kitty != 0 {
        out.fmt(format_args!("\x1b[>{kitty}u"));
    }
    for (bit, set) in [
        (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
        (TermMode::UTF8_MOUSE, "\x1b[?1005h"),
        (TermMode::SGR_MOUSE, "\x1b[?1006h"),
        (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
        (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
        (TermMode::APP_CURSOR, "\x1b[?1h"),
        (TermMode::APP_KEYPAD, "\x1b="),
        (TermMode::INSERT, "\x1b[4h"),
        (TermMode::LINE_FEED_NEW_LINE, "\x1b[20h"),
    ] {
        if mode.contains(bit) {
            out.raw(set);
        }
    }
    for (bit, reset) in [
        (TermMode::LINE_WRAP, "\x1b[?7l"),
        (TermMode::ALTERNATE_SCROLL, "\x1b[?1007l"),
        (TermMode::URGENCY_HINTS, "\x1b[?1042l"),
        (TermMode::SHOW_CURSOR, "\x1b[?25l"),
    ] {
        if !mode.contains(bit) {
            out.raw(reset);
        }
    }
    let style = term.cursor_style();
    if style != CursorStyle::default() {
        let base = match style.shape {
            CursorShape::Underline => Some(3),
            CursorShape::Beam => Some(5),
            CursorShape::Block => Some(1),
            CursorShape::HollowBlock | CursorShape::Hidden => None,
        };
        if let Some(base) = base {
            out.fmt(format_args!(
                "\x1b[{} q",
                base + usize::from(!style.blinking)
            ));
        }
    }
}
