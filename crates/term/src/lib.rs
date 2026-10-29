//! A terminal screen fed with PTY bytes. See decision 0007 in `docs/architecture.md`.
//!
//! [`Screen`] wraps `alacritty_terminal` and holds no policy: it parses output, answers terminal queries
//! ([`Screen::take_replies`]), reports the input modes the program asked for ([`Screen::modes`]), exposes the visible
//! cells read-only, and can redraw itself as an escape sequence ([`Screen::snapshot_ansi`]). It reads no clock: the
//! synchronized-update timeout is driven by the caller through [`Screen::sync_hold`] and [`Screen::expire_sync`].

mod convert;
mod replies;
mod snapshot;
mod sync;
mod types;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

pub use types::*;

use replies::Listener;
use sync::ManualTimeout;

/// Primary-screen lines kept above the viewport, so reflow on resize can bring them back.
const SCROLLBACK_LINES: usize = 1000;

/// One terminal: the grid, its modes, and the parser state between `feed` calls.
pub struct Screen {
    term: Term<Listener>,
    parser: Processor<ManualTimeout>,
    /// The same queue the `Term` sends its events to.
    events: Listener,
    replies: Vec<u8>,
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Size {
    /// alacritty needs at least one cell.
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

impl Screen {
    /// A blank screen. Sizes of zero are clamped to one.
    pub fn new(cols: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            kitty_keyboard: true,
            ..Config::default()
        };
        let events = Listener::default();
        Self {
            term: Term::new(config, &Size::new(cols, rows), events.clone()),
            parser: Processor::new(),
            events,
            replies: Vec::new(),
        }
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (
            to_u16(self.term.columns()),
            to_u16(self.term.screen_lines()),
        )
    }

    /// Parses PTY output. Escape sequences and UTF-8 may be split across calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.collect_replies();
    }

    /// Resizes the grid, reflowing the primary screen. Sizes of zero are clamped to one.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size::new(cols, rows));
    }

    /// Bytes to write back to the PTY: answers to queries such as DA1, cursor position, kitty keyboard flags and
    /// OSC 10/11 colours, in the order they were asked.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn modes(&self) -> Modes {
        convert::modes(*self.term.mode())
    }

    pub fn cursor(&self) -> Cursor {
        let point = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        Cursor {
            col: to_u16(point.column.0),
            row: to_u16(usize::try_from(point.line.0).unwrap_or(0)),
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            shape: convert::cursor_shape(style.shape),
            blinking: style.blinking,
        }
    }

    /// The cells of a visible row, left to right. Empty if `row` is off the screen.
    pub fn row(&self, row: u16) -> impl Iterator<Item = Cell<'_>> {
        let (cols, rows) = self.size();
        let cols = if row < rows { cols } else { 0 };
        (0..cols).filter_map(move |col| self.cell(col, row))
    }

    pub fn cell(&self, col: u16, row: u16) -> Option<Cell<'_>> {
        let (cols, rows) = self.size();
        if col >= cols || row >= rows {
            return None;
        }
        let cell = &self.term.grid()[Point::new(Line(i32::from(row)), Column(usize::from(col)))];
        Some(convert::cell(cell))
    }

    /// `Some` while output is held by an open synchronized update. See [`SyncHold`].
    pub fn sync_hold(&self) -> Option<SyncHold> {
        self.parser.sync_timeout().hold()
    }

    /// The caller's timer for [`SyncHold::id`] fired: draw the held output now. Does nothing if that update already
    /// ended or a newer begin marker restarted the timeout.
    pub fn expire_sync(&mut self, id: u64) {
        if self.sync_hold().is_some_and(|hold| hold.id == id) {
            self.parser.stop_sync(&mut self.term);
            self.collect_replies();
        }
    }

    /// An escape sequence that redraws this screen. Fed into a screen of the same size, it reproduces the visible
    /// cells, cursor, input modes and pen, so a client attaching mid-session can then follow live output.
    ///
    /// Not carried over: the primary screen while the alt screen is active, scrollback, the scroll region, tab
    /// stops, hyperlinks, the title, and output still held by a synchronized update.
    pub fn snapshot_ansi(&self) -> Vec<u8> {
        snapshot::ansi(&self.term)
    }

    fn collect_replies(&mut self) {
        let answers = replies::answer(self.events.drain(), self.term.colors());
        self.replies.extend(answers);
    }
}

fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}
