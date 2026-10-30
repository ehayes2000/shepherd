//! Serializes a screen as escape sequences that redraw it in a fresh (or reset) screen of the same size.

use std::io::Write;

use alacritty_terminal::grid::{Cursor, Dimensions, Row};
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

/// `kitty_top` is the top of the active screen's kitty keyboard stack, 0 when it is empty.
pub(crate) fn ansi<T>(term: &Term<T>, kitty_top: u8) -> Vec<u8> {
    let mut out = Writer {
        buf: Vec::new(),
        pen: Pen::of(&Cell::default()),
    };
    let mode = *term.mode();

    // RIS, so the target starts from the same blank state whether it is fresh or not.
    out.raw("\x1bc");
    palette(&mut out, term);
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
    modes(&mut out, term, kitty_top);
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

    /// DECRC: back to the cursor (and pen) saved by DECSC when the pen was `pen`.
    fn restore(&mut self, pen: Pen) {
        self.raw("\x1b8");
        self.pen = pen;
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
        // A leading spacer is re-created by printing the wide char that starts the next row, but only when that
        // row is a continuation starting with a wide char drawn with the same pen. Otherwise draw it as a blank.
        let next = (wraps && row + 1 < grid.screen_lines())
            .then(|| &grid[Line(row as i32 + 1)][Column(0)]);
        let recreated = next.is_some_and(|next| next.flags.contains(Flags::WIDE_CHAR));
        if continues {
            // The target's cursor is waiting to wrap from the row above; printing one cell makes it wrap.
            end = end.max(1);
        } else if end > 0 {
            out.goto(row, 0);
        }
        for col in 0..end {
            let cell = &line[Column(col)];
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                && col + 1 == cols
                && cell.c == ' '
                && recreated
                && next.is_some_and(|next| Pen::of(next) == Pen::of(cell))
            {
                continue;
            }
            draw_cell(out, row, line, col, 0, false);
        }
        continues = wraps;
    }
}

/// Draws `line[col]` at column `col - shift`, where the target's cursor is. Wide-char halves that lost their other
/// half (ECH, EL, ICH or DCH over one) are rebuilt with the same edits. `shift` is 1 while redrawing the end of a
/// row one column early for [`last_column`]. `nested` means a caller has saved the cursor, so this must not.
fn draw_cell(
    out: &mut Writer,
    row: usize,
    line: &Row<Cell>,
    col: usize,
    shift: usize,
    nested: bool,
) {
    let cols = line.len();
    let at = col - shift;
    let cell = &line[Column(col)];
    let wide_before = col > 0 && line[Column(col - 1)].flags.contains(Flags::WIDE_CHAR);
    let spacer_after = col + 1 < cols
        && line[Column(col + 1)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER);
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        if !wide_before {
            if at + 1 < cols {
                // Draw a wide char here and delete its first half, which pulls the spacer into this column. Only
                // blank, not-yet-drawn cells shift in behind it.
                out.put(&Cell {
                    c: FILLER,
                    ..cell.clone()
                });
                out.goto(row, at);
                out.pen(&Pen::of(&Cell::default()));
                out.raw("\x1b[P");
                out.goto(row, at + 1);
            } else {
                last_column(out, row, line, col);
            }
        }
        // Otherwise it was written together with its wide char. A tab over a spacer keeps the spacer.
        if cell.c == '\t' {
            tab_at(out, row, at, !nested);
        }
        return;
    }
    if cell.flags.contains(Flags::WIDE_CHAR) && !spacer_after {
        if at + 1 < cols {
            // Draw it, then erase its spacer.
            out.put(cell);
            out.goto(row, at + 1);
            out.pen(&Pen::of(&Cell::default()));
            out.raw("\x1b[X");
        } else {
            last_column(out, row, line, col);
        }
        return;
    }
    if cell.c == '\t' {
        // A tab moves the cursor instead of printing. Draw the cell's style with a space, then mark it as a tab.
        out.put(&Cell {
            c: ' ',
            ..cell.clone()
        });
        tab_at(out, row, at, !nested);
        return;
    }
    out.put(cell);
}

/// A broken wide-char half in the last column, where there's no room to draw a wide char. Picks the last cell `p`
/// before it that is plain or starts a whole wide char, redraws everything after that one column early, then
/// erases `p`, shifts the rest into place with ICH at `p`, and redraws `p`. The cursor is saved with a pending wrap
/// first, so a row that wraps still does. With no such `p`, the cell comes out as a blank with the same style.
fn last_column(out: &mut Writer, row: usize, line: &Row<Cell>, col: usize) {
    let cell = &line[Column(col)];
    // A leading spacer away from the last column is just a blank; redrawing it as one loses nothing visible.
    let narrow = |p: usize| {
        let cell = &line[Column(p)];
        plain(cell) || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
    };
    let starts_whole = |p: usize| {
        narrow(p)
            || (line[Column(p)].flags.contains(Flags::WIDE_CHAR)
                && p + 1 < col
                && line[Column(p + 1)].flags.contains(Flags::WIDE_CHAR_SPACER))
    };
    let Some(p) = (0..col).rev().find(|&p| starts_whole(p)) else {
        out.put(&blank_like(cell));
        return;
    };
    let rest = if narrow(p) { p + 1 } else { p + 2 };
    // A narrow char in the last column leaves the cursor waiting to wrap; save that. ICH shifts it out below.
    out.goto(row, col);
    out.put(&blank_like(cell));
    out.raw("\x1b7");
    let pen = out.pen.clone();
    out.goto(row, rest - 1);
    for src in rest..=col {
        draw_cell(out, row, line, src, 1, true);
    }
    out.goto(row, p);
    out.pen(&Pen::of(&Cell::default()));
    if rest == p + 2 {
        // The old wide char at `p`, whose spacer the redraw overwrote.
        out.raw("\x1b[X");
    }
    out.raw("\x1b[@");
    draw_cell(out, row, line, p, 0, true);
    out.restore(pen);
}

/// Turns the blank at `(row, col)` into a tab cell, which is what a tab does to a blank it passes over, leaving the
/// cursor just after it. With `keep_wrap` it saves and restores the cursor instead, so a pending wrap survives.
fn tab_at(out: &mut Writer, row: usize, col: usize, keep_wrap: bool) {
    if keep_wrap {
        out.raw("\x1b7");
    }
    out.goto(row, col);
    out.raw("\t");
    if keep_wrap {
        out.raw("\x1b8");
    } else {
        out.goto(row, col + 1);
    }
}

/// A cell with no wide-char bookkeeping, safe to write next to a broken half without disturbing it.
fn plain(cell: &Cell) -> bool {
    !cell
        .flags
        .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

/// Any two-column character, used to create a spacer whose own wide char is then deleted.
const FILLER: char = '\u{3000}';

/// A one-column blank with the same style as `cell`.
fn blank_like(cell: &Cell) -> Cell {
    let mut blank = Cell {
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags & STYLE,
        ..Cell::default()
    };
    blank.set_underline_color(cell.underline_color());
    blank
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
    // A widening resize can leave the wrap flag set away from the last column, where the next character or tab
    // still starts a new line. No sequence puts a cursor in that state, so it is dropped (documented on
    // `Screen::snapshot_ansi`).
    if !grid.cursor.input_needs_wrap || col + 1 != term.columns() {
        out.goto(row, col);
        return;
    }
    // The cursor sits past the last column. Redrawing the last cell puts the target in the same state. For a tab or
    // a broken wide-char half that uses DECSC/DECRC, which replaces the saved cursor set just before.
    let line = &grid[Line(row as i32)];
    let start = if col > 0
        && line[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER)
        && line[Column(col - 1)].flags.contains(Flags::WIDE_CHAR)
    {
        col - 1
    } else {
        col
    };
    out.goto(row, start);
    draw_cell(out, row, line, start, 0, false);
}

fn clamped<T, C>(term: &Term<T>, cursor: &Cursor<C>) -> (usize, usize) {
    let row = usize::try_from(cursor.point.line.0)
        .unwrap_or(0)
        .min(term.screen_lines() - 1);
    let col = cursor.point.column.0.min(term.columns() - 1);
    (row, col)
}

/// Colours set with OSC 4 / 10 / 11 / 12. RIS keeps them, so the target's own are reset first.
fn palette<T>(out: &mut Writer, term: &Term<T>) {
    out.raw("\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07");
    let colors = term.colors();
    for index in 0..=NamedColor::Cursor as usize {
        let Some(c) = colors[index] else {
            continue;
        };
        if index < 256 {
            out.fmt(format_args!("\x1b]4;{index}"));
        } else {
            // Foreground, background and cursor are OSC 10, 11 and 12.
            out.fmt(format_args!(
                "\x1b]{}",
                10 + index - NamedColor::Foreground as usize
            ));
        }
        out.fmt(format_args!(";rgb:{:02x}/{:02x}/{:02x}\x07", c.r, c.g, c.b));
    }
}

/// Pushes the top of the kitty keyboard stack, then sets the active flags over it if they differ, since
/// `CSI = flags u` changes only the active flags.
fn kitty(out: &mut Writer, mode: TermMode, top: u8) {
    let active = kitty_flags(mode);
    if top != 0 {
        out.fmt(format_args!("\x1b[>{top}u"));
    }
    if active != top {
        out.fmt(format_args!("\x1b[={active};1u"));
    }
}

fn modes<T>(out: &mut Writer, term: &Term<T>, kitty_top: u8) {
    let mode = *term.mode();
    kitty(out, mode, kitty_top);
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
