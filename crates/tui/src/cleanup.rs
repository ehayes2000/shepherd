//! The cleanup view (T25b): a header line, then one row per orphan worktree (`⎇` and its branch) or stale session (`✕`
//! and its name), each with its status columns (uncommitted changes, where its commits are) and what removing it would
//! lose or how removing it went; then a hint with `[Remove safe]` (while any row is safe) and `[Close]`. While the list
//! loads, can't be listed, or is empty, the first row says so.
//!
//! It is as tall as its rows and fits the window. The rows shown start at the client's [`Client::cleanup_scroll`],
//! moved up as far as the last rows fill it. [`place`] says where it and its buttons are, for both drawing and
//! [`crate::hit`].

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use shepherd_core::client::{
    CleanupCheck, CleanupItem, CleanupListing, CleanupOutcome, CleanupRow, Client,
};
use shepherd_core::ports::WorktreeStatus;

use crate::draw::{self, shown};
use crate::layout::Target;
use crate::popup::{Placed, centered, dialog, inner};

/// The view's width, its border included.
const WIDTH: u16 = 76;
/// Lines besides the rows: the border, the header and the buttons.
const CHROME: u16 = 4;
/// The widest the name column is.
const NAME_WIDTH: u16 = 22;
/// The changes column's width, and the commits column's, each with the blank after it.
const CHANGES_WIDTH: u16 = 8;
const COMMITS_WIDTH: u16 = 12;
const HEADER: [&str; 4] = ["worktree", "changes", "commits", "if removed"];
const HINT: &str = "ignored files are removed too";
const LOADING: &str = "loading worktrees…";
const NOTHING: &str = "Nothing to clean up.";
/// An orphan worktree's glyph, and a stale session's.
const WORKTREE: &str = "⎇";
const SESSION: &str = "✕";

/// Where the view and its buttons are in `window`.
pub(crate) fn place(client: &Client, window: Rect) -> Placed {
    let rows = u16::try_from(client.cleanup_rows().len().max(1)).unwrap_or(u16::MAX);
    let area = centered(window, WIDTH, rows.saturating_add(CHROME));
    let buttons: &[(&'static str, Target)] = if client.cleanup_safe() > 0 {
        &[
            ("[Remove safe]", Target::CleanupRemoveSafe),
            ("[Close]", Target::Dismiss),
        ]
    } else {
        &[("[Close]", Target::Dismiss)]
    };
    dialog(area, buttons)
}

/// The rows' lines inside the view's box `inside`.
fn row_lines(inside: Rect) -> std::ops::Range<u16> {
    inside.y.saturating_add(1)..inside.bottom().saturating_sub(1)
}

/// How many rows the open view shows at once in `window`.
pub(crate) fn rows(client: &Client, window: Rect) -> usize {
    row_lines(inner(place(client, window).area)).len()
}

/// The columns' left edges in a row starting at `x`: the glyph, the name, the changes, the commits and the rest.
fn columns(x: u16) -> [u16; 5] {
    let name = x.saturating_add(3);
    let changes = name.saturating_add(NAME_WIDTH + 1);
    let commits = changes.saturating_add(CHANGES_WIDTH);
    let state = commits.saturating_add(COMMITS_WIDTH);
    [x.saturating_add(1), name, changes, commits, state]
}

/// Draws `text` at column `x` of `line`, at most `width` columns and no further than the line's end, cut with `…`.
fn cell(buf: &mut Buffer, line: Rect, x: u16, width: u16, text: &str, style: Style) {
    if x >= line.right() {
        return;
    }
    let room = line.right().saturating_sub(x).min(width);
    draw::set_cut(buf, x, line.y, &shown(text), usize::from(room), style);
}

/// Draws the view's text in `placed`: the header, the rows (or why there are none) and the hint.
pub(crate) fn render(client: &Client, placed: &Placed, buf: &mut Buffer) {
    let inside = inner(placed.area);
    if inside.width < 4 || inside.height == 0 {
        return;
    }
    let gray = Style::new().fg(Color::Gray);
    let header = Rect {
        height: 1,
        ..inside
    };
    let [_, name, changes, commits, state] = columns(inside.x);
    let dim = gray.add_modifier(Modifier::DIM);
    for (x, width, title) in [
        (name, NAME_WIDTH, HEADER[0]),
        (changes, CHANGES_WIDTH, HEADER[1]),
        (commits, COMMITS_WIDTH, HEADER[2]),
        (state, u16::MAX, HEADER[3]),
    ] {
        cell(buf, header, x, width, title, dim);
    }
    let lines = row_lines(inside);
    let all = client.cleanup_rows();
    let first = client
        .cleanup_scroll()
        .min(all.len().saturating_sub(lines.len()));
    for (row, y) in all.iter().skip(first).zip(lines.clone()) {
        draw_row(row, Rect::new(inside.x, y, inside.width, 1), buf);
    }
    if all.is_empty() && !lines.is_empty() {
        let (text, style) = match client.cleanup_listing() {
            Some(CleanupListing::Loading) | None => (LOADING.to_owned(), gray),
            Some(CleanupListing::Failed(why)) => (
                format!("can't list worktrees: {why}"),
                Style::new().fg(Color::Red),
            ),
            Some(CleanupListing::Listed) => (NOTHING.to_owned(), gray),
        };
        let line = Rect::new(inside.x, lines.start, inside.width, 1);
        cell(buf, line, name, u16::MAX, &text, style);
    }
    if inside.height >= 2 {
        // Left of the buttons, with a blank column before them.
        let buttons = placed
            .controls
            .iter()
            .map(|c| c.area.x)
            .min()
            .unwrap_or(inside.right());
        let line = Rect::new(inside.x, inside.bottom() - 1, inside.width, 1);
        let room = buttons.saturating_sub(inside.x + 2);
        cell(buf, line, inside.x + 1, room, HINT, dim);
    }
}

/// Draws one row in `line`.
fn draw_row(row: &CleanupRow, line: Rect, buf: &mut Buffer) {
    let [glyph, name, changes, commits, state] = columns(line.x);
    let (symbol, color, label) = match &row.item {
        CleanupItem::Orphan(orphan) => {
            let label = match &orphan.branch {
                Some(branch) => branch.clone(),
                None => format!("{} (detached)", dir_name(&orphan.path)),
            };
            (WORKTREE, Color::Cyan, label)
        }
        CleanupItem::Session { name, .. } => (SESSION, Color::Red, name.clone()),
    };
    cell(buf, line, glyph, 1, symbol, Style::new().fg(color));
    cell(buf, line, name, NAME_WIDTH, &label, Style::new());
    let gray = Style::new().fg(Color::Gray);
    if let CleanupCheck::Checked(status) = &row.check {
        let dirty = if status.dirty { "dirty" } else { "clean" };
        let style = if status.dirty {
            Style::new().fg(Color::Yellow)
        } else {
            gray
        };
        cell(buf, line, changes, CHANGES_WIDTH - 1, dirty, style);
        cell(
            buf,
            line,
            commits,
            COMMITS_WIDTH - 1,
            &commits_of(status),
            gray,
        );
    }
    let (text, style) = state_of(row);
    cell(buf, line, state, u16::MAX, &text, style);
}

/// Where a worktree's commits are.
fn commits_of(status: &WorktreeStatus) -> String {
    if status.merged_into_default {
        "merged".to_owned()
    } else if status.pushed {
        "pushed".to_owned()
    } else {
        format!("{} unpushed", status.ahead)
    }
}

/// What a row's last column says: how removing it went, else what removing it would lose.
fn state_of(row: &CleanupRow) -> (String, Style) {
    let gray = Style::new().fg(Color::Gray);
    let green = Style::new().fg(Color::Green);
    let yellow = Style::new().fg(Color::Yellow);
    match (&row.outcome, &row.check) {
        (Some(CleanupOutcome::Queued), _) => ("queued".to_owned(), gray),
        (Some(CleanupOutcome::Removing), _) => ("removing…".to_owned(), gray),
        (Some(CleanupOutcome::Removed), _) => ("removed".to_owned(), green),
        (Some(CleanupOutcome::Gone), _) => ("already gone".to_owned(), gray),
        (Some(CleanupOutcome::Kept(why)), _) => (format!("kept: {why}"), yellow),
        (Some(CleanupOutcome::Unknown), _) => {
            ("outcome unknown (connection dropped)".to_owned(), yellow)
        }
        (None, CleanupCheck::Checking) => ("checking…".to_owned(), gray),
        (None, CleanupCheck::Failed(why)) => {
            (format!("can't check: {why}"), Style::new().fg(Color::Red))
        }
        (None, CleanupCheck::Checked(status)) => {
            let losses = status.losses();
            if losses.is_empty() {
                ("safe".to_owned(), green)
            } else {
                let lost: Vec<String> = losses.iter().map(ToString::to_string).collect();
                (format!("loses {}", lost.join(", ")), yellow)
            }
        }
    }
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
