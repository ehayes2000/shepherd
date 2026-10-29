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
mod unfinished;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

pub use types::*;

use replies::{Listener, ReportedColors};
use sync::ManualTimeout;
use unfinished::Unfinished;

/// Smallest grid: alacritty needs two columns to place a wide character.
pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 1;
/// Largest grid. Bigger sizes are clamped so a bogus resize can't allocate gigabytes.
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

/// Primary-screen lines kept above the viewport, so reflow on resize can bring them back.
const SCROLLBACK_LINES: usize = 1000;

/// One terminal: the grid, its modes, and the parser state between `feed` calls.
pub struct Screen {
    term: Term<Listener>,
    parser: Processor<ManualTimeout>,
    /// The same queue the `Term` sends its events to.
    events: Listener,
    replies: Vec<u8>,
    /// A copy of the bytes the parser is holding for an open synchronized update, so a snapshot can carry them.
    /// alacritty keeps them private; they are always the tail of what was fed.
    held: Vec<u8>,
    /// The escape sequence or UTF-8 char the parser is in the middle of, so a snapshot can carry it.
    unfinished: Unfinished,
    reported: ReportedColors,
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Size {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: usize::from(cols.clamp(MIN_COLS, MAX_COLS)),
            rows: usize::from(rows.clamp(MIN_ROWS, MAX_ROWS)),
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
    /// A blank screen. `cols` is clamped to `MIN_COLS..=MAX_COLS` and `rows` to `MIN_ROWS..=MAX_ROWS`.
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
            held: Vec::new(),
            unfinished: Unfinished::default(),
            reported: ReportedColors::default(),
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
        self.track_parsed(bytes);
        self.collect_replies();
    }

    /// Resizes the grid, reflowing the primary screen. Sizes are clamped as in [`Screen::new`].
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size::new(cols, rows));
    }

    /// Bytes to write back to the PTY: answers to queries such as DA1, cursor position, kitty keyboard flags and
    /// OSC 10/11 colours, in the order they were asked.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// The default foreground and background reported to OSC 10 / 11 queries when the program hasn't set its
    /// own. Starts as white on black.
    pub fn set_reported_colors(&mut self, fg: (u8, u8, u8), bg: (u8, u8, u8)) {
        self.reported = ReportedColors { fg, bg };
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
            let held = std::mem::take(&mut self.held);
            self.unfinished.advance(&held);
            self.collect_replies();
        }
    }

    /// An escape sequence that redraws this screen. Fed into a screen of the same size, it reproduces the visible
    /// cells, cursor, input modes and pen, so a client attaching mid-session can then follow live output.
    ///
    /// Output held by an open synchronized update is appended after a begin marker, so the copy is left holding
    /// the same bytes. An escape sequence or UTF-8 char the parser is in the middle of is carried too, so the copy
    /// reads the rest of it the same way. Past 64 KiB only the bytes that change where a sequence ends are carried,
    /// so its parameters or payload may be cut short.
    ///
    /// Not carried over: the primary screen while the alt screen is active, scrollback, the scroll region, tab
    /// stops, hyperlinks, the title, the charset state, colours set with OSC 4 / 10 / 11 (so colour queries can get
    /// different answers), the last printed character that REP repeats, and whether the saved cursor (DECSC) was
    /// waiting to wrap. Only the top of the active screen's kitty keyboard stack is re-pushed; deeper entries and the
    /// inactive screen's stack are lost, and flags set with `CSI = u` on an empty stack are pushed, so the copy
    /// reports them to `CSI ? u` where the original reports 0. A widening resize can leave the cursor
    /// waiting to wrap away from the last column, so the next character or tab starts a new line; the copy's cursor
    /// doesn't wait. After a widening resize alacritty can also stop resetting the blank cells of a row erased with
    /// a background colour, so a later erase, insert or reset leaves that colour in the original but not the copy.
    ///
    /// Wide characters with a half erased (by ECH, EL, ICH or DCH) are rebuilt with the same edits, with two gaps:
    /// a broken half in the last column with only broken halves before it comes out as a blank, and when the
    /// cursor is waiting to wrap after a tab or broken half in the last column, the saved cursor (DECSC) is lost.
    pub fn snapshot_ansi(&self) -> Vec<u8> {
        let mut out = snapshot::ansi(&self.term);
        if self.sync_hold().is_some() {
            // The parser reads the unfinished bytes before the held ones when the update ends, so the copy holds
            // them too. Fed before the marker, a broken UTF-8 char would be drawn while the original still holds it.
            out.extend_from_slice(b"\x1b[?2026h");
            out.extend_from_slice(self.unfinished.bytes());
            out.extend_from_slice(&self.held);
        } else {
            out.extend_from_slice(self.unfinished.bytes());
        }
        out
    }

    /// Bytes fed to the extra parsers that follow unfinished sequences for [`Screen::snapshot_ansi`]. Only for tests
    /// that check this work stays linear in the output.
    #[doc(hidden)]
    pub fn tracking_work(&self) -> u64 {
        self.unfinished.work()
    }

    /// Splits what was not yet parsed before this feed (`held`, then `bytes`) into what the parser has now read
    /// and what an open synchronized update still holds. The parser always reads a prefix of it.
    fn track_parsed(&mut self, bytes: &[u8]) {
        let keep = match self.sync_hold() {
            Some(_) => self.parser.sync_bytes_count(),
            None => 0,
        };
        if self.held.is_empty() {
            let split = bytes.len().saturating_sub(keep);
            self.unfinished.advance(&bytes[..split]);
            self.held.extend_from_slice(&bytes[split..]);
        } else {
            self.held.extend_from_slice(bytes);
            let split = self.held.len().saturating_sub(keep);
            let parsed: Vec<u8> = self.held.drain(..split).collect();
            self.unfinished.advance(&parsed);
        }
    }

    fn collect_replies(&mut self) {
        let answers = replies::answer(self.events.drain(), self.term.colors(), &self.reported);
        self.replies.extend(answers);
    }
}

fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}
