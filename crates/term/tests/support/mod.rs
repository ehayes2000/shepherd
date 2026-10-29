//! Loads the recorded PTY fixtures in `tests/fixtures` (see its README). Also used by `shepherd-tui`'s tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use shepherd_term::{Cell, Cursor, Modes, Screen};

pub const AGENTS: [&str; 2] = ["claude", "codex"];

/// One recorded segment: bytes produced at one terminal size.
pub struct Segment {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub bytes: Vec<u8>,
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../term/tests/fixtures")
}

/// The segments of `<agent>-session.*.bin`, in recording order.
pub fn segments(agent: &str) -> Vec<Segment> {
    let prefix = format!("{agent}-session.");
    let mut found: Vec<(u32, Segment)> = std::fs::read_dir(fixtures_dir())
        .unwrap_or_else(|e| panic!("fixtures dir: {e}"))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let rest = name.strip_prefix(&prefix)?.strip_suffix(".bin")?;
            let (index, _) = rest.split_once('-')?;
            let (_, size) = rest.rsplit_once('-')?;
            let (cols, rows) = size.split_once('x')?;
            let bytes = std::fs::read(entry.path()).ok()?;
            let segment = Segment {
                name: name.trim_end_matches(".bin").to_owned(),
                cols: cols.parse().ok()?,
                rows: rows.parse().ok()?,
                bytes,
            };
            Some((index.parse().ok()?, segment))
        })
        .collect();
    found.sort_by_key(|(index, _)| *index);
    assert!(!found.is_empty(), "no fixtures for {agent}");
    found.into_iter().map(|(_, segment)| segment).collect()
}

/// Replays `segments` into a new screen, resizing before each one, feeding `chunk` bytes at a time.
pub fn replay(segments: &[Segment], chunk: usize) -> Screen {
    let first = &segments[0];
    let mut screen = Screen::new(first.cols, first.rows);
    for segment in segments {
        screen.resize(segment.cols, segment.rows);
        for piece in segment.bytes.chunks(chunk.max(1)) {
            screen.feed(piece);
        }
    }
    screen
}

/// Everything a viewer can observe about a screen, for equality checks.
#[derive(Debug, PartialEq)]
pub struct Observed<'a> {
    pub size: (u16, u16),
    pub cells: Vec<Vec<Cell<'a>>>,
    pub cursor: Cursor,
    pub modes: Modes,
}

pub fn observe(screen: &Screen) -> Observed<'_> {
    let (cols, rows) = screen.size();
    Observed {
        size: (cols, rows),
        cells: (0..rows).map(|row| screen.row(row).collect()).collect(),
        cursor: screen.cursor(),
        modes: screen.modes(),
    }
}

/// The visible text, one string per row, with spacer cells skipped.
pub fn text(screen: &Screen) -> Vec<String> {
    let (_, rows) = screen.size();
    (0..rows)
        .map(|row| {
            screen
                .row(row)
                .filter(|cell| cell.width != shepherd_term::Width::Spacer)
                .map(|cell| cell.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}
