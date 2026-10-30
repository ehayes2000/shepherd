//! The finder popup (T26): what has been typed, then one row per result (sessions with their status glyph, name and
//! branch and workspace; orphan worktrees with `⎇`, their branch and directory), the first highlighted since enter opens
//! it, then a line saying the worktrees are loading or why they couldn't be listed, and the hint with `[Close]`.
//!
//! It is as tall as its results and fits the window, its top two lines down (or as far as fits), so it doesn't move
//! while typing changes its height. The rows shown start at the client's [`Client::finder_scroll`],
//! moved up as far as the last rows fill it. [`place`] says where the rows are, for both drawing and [`crate::hit`].

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use shepherd_core::client::{Client, FinderItem, Orphan, Orphans};

use crate::draw::{self, shown};
use crate::layout::Target;
use crate::popup::{Control, Placed, centered, dialog, inner, text_input};
use crate::sidebar::glyph;
use crate::width::columns;

/// The finder's width, its border included.
const WIDTH: u16 = 56;
/// How far below the window's top the finder is, when it fits.
const TOP: u16 = 2;
/// Lines besides the rows and the status: the border, the input and the buttons.
const CHROME: u16 = 4;
/// What the input line starts with.
const PROMPT: &str = "⌕ ";
/// What the empty input says.
const PLACEHOLDER: &str = "type to find a session or worktree";
const HINT: &str = "enter opens the first · esc closes";
const LOADING: &str = "loading worktrees…";
const NO_MATCHES: &str = "No matches.";
const NOTHING: &str = "No sessions or worktrees yet.";
/// An orphan worktree's glyph.
const WORKTREE: &str = "⎇";
/// The widest a name is drawn when something follows it.
const NAME_MAX: usize = 24;
/// The first result's background: what enter opens.
const FIRST_BG: Color = Color::DarkGray;

/// The line below the rows, while the worktrees are loading or couldn't be listed.
fn status(client: &Client) -> Option<(String, Style)> {
    match client.orphans()? {
        Orphans::Loading => Some((LOADING.to_owned(), Style::new().fg(Color::Gray))),
        Orphans::Failed(why) => Some((
            format!("can't list worktrees: {why}"),
            Style::new().fg(Color::Red),
        )),
        Orphans::Listed(_) => None,
    }
}

/// The rows' lines inside the finder's box `inside`, with a status line below them or not.
fn row_lines(inside: Rect, status: bool) -> std::ops::Range<u16> {
    let below = 1 + u16::from(status);
    inside.y.saturating_add(1)..inside.bottom().saturating_sub(below)
}

/// Where the finder and its rows are in `window`.
pub(crate) fn place(client: &Client, window: Rect) -> Placed {
    let results = client.finder();
    let status = status(client).is_some();
    let list = u16::try_from(results.len().max(1)).unwrap_or(u16::MAX);
    let height = list
        .saturating_add(u16::from(status))
        .saturating_add(CHROME);
    // Its top stays put while typing changes its height.
    let area = centered(window, WIDTH, height);
    let area = Rect {
        y: window.y + TOP.min(window.height - area.height),
        ..area
    };
    let mut placed = dialog(area, &[("[Close]", Target::Dismiss)]);
    let inside = inner(placed.area);
    let lines = row_lines(inside, status);
    let first = client
        .finder_scroll()
        .min(results.len().saturating_sub(lines.len()));
    for (item, y) in results.into_iter().skip(first).zip(lines) {
        placed.controls.push(Control {
            area: Rect::new(inside.x, y, inside.width, 1),
            label: shown(&label(client, &item)),
            target: Target::FinderRow(item),
        });
    }
    placed
}

/// How many results the open finder shows at once in `window`.
pub(crate) fn rows(client: &Client, window: Rect) -> usize {
    let placed = place(client, window);
    row_lines(inner(placed.area), status(client).is_some()).len()
}

/// A row's name and what follows it.
fn parts(client: &Client, item: &FinderItem) -> (String, String) {
    match item {
        FinderItem::Session(id) => {
            let Some(s) = client.sessions().iter().find(|s| &s.id == id) else {
                return (id.to_string(), String::new());
            };
            let workspace = s.workspace.as_deref().map(dir_name);
            let detail: Vec<String> = s.branch.iter().cloned().chain(workspace).collect();
            (s.name.clone(), detail.join(" · "))
        }
        FinderItem::Worktree(Orphan { path, branch }) => match branch {
            Some(branch) => (branch.clone(), format!("{} · no session", dir_name(path))),
            None => (dir_name(path), "detached · no session".to_owned()),
        },
    }
}

/// A row's text, as one line: for its control.
fn label(client: &Client, item: &FinderItem) -> String {
    let (name, detail) = parts(client, item);
    format!("{name}  {detail}")
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Draws the finder's text in `placed`: the input, the rows, the status and the hint. Returns where the cursor goes.
pub(crate) fn render(
    client: &Client,
    query: &str,
    placed: &Placed,
    buf: &mut Buffer,
) -> Option<Position> {
    let inside = inner(placed.area);
    if inside.width < 4 || inside.height == 0 {
        return None;
    }
    let gray = Style::new().fg(Color::Gray);
    let width = usize::from(inside.width);
    let (end, _) = draw::set(buf, inside.x, inside.y, &shown(PROMPT), width, gray);
    let input = Rect::new(end, inside.y, inside.right().saturating_sub(end), 1);
    let cursor = text_input(query, input, buf);
    if query.is_empty() {
        let room = usize::from(input.width.saturating_sub(1));
        let placeholder = gray.add_modifier(Modifier::DIM);
        draw::set(
            buf,
            input.x,
            input.y,
            &shown(PLACEHOLDER),
            room,
            placeholder,
        );
    }
    let results = client.finder();
    let rows: Vec<&Control> = placed
        .controls
        .iter()
        .filter(|c| matches!(c.target, Target::FinderRow(_)))
        .collect();
    for control in &rows {
        if let Target::FinderRow(item) = &control.target {
            if results.first() == Some(item) {
                buf.set_style(control.area, Style::new().bg(FIRST_BG));
            }
            row(client, item, control.area, buf);
        }
    }
    let status = status(client);
    let lines = row_lines(inside, status.is_some());
    if results.is_empty() && !lines.is_empty() {
        let empty = if query.trim().is_empty() {
            NOTHING
        } else {
            NO_MATCHES
        };
        draw::set(
            buf,
            inside.x + 1,
            lines.start,
            &shown(empty),
            width - 1,
            gray,
        );
    }
    if let Some((text, style)) = status
        && inside.height >= 3
    {
        let y = inside.bottom() - 2;
        draw::set_cut(buf, inside.x + 1, y, &shown(&text), width - 1, style);
    }
    if inside.height >= 2 {
        // Left of the buttons, with a blank column before them.
        let buttons = placed
            .controls
            .iter()
            .filter(|c| c.target == Target::Dismiss)
            .map(|c| c.area.x)
            .min()
            .unwrap_or(inside.right());
        let room = usize::from(buttons.saturating_sub(inside.x + 2));
        let hint = gray.add_modifier(Modifier::DIM);
        draw::set(
            buf,
            inside.x + 1,
            inside.bottom() - 1,
            &shown(HINT),
            room,
            hint,
        );
    }
    cursor
}

/// Draws one result in `area`: its glyph, its name, and what follows dimmed, cut to fit.
fn row(client: &Client, item: &FinderItem, area: Rect, buf: &mut Buffer) {
    if area.width < 5 {
        return;
    }
    let (symbol, color) = match item {
        FinderItem::Session(id) => client
            .sessions()
            .iter()
            .find(|s| &s.id == id)
            .map_or(("·", Color::DarkGray), glyph),
        FinderItem::Worktree(_) => (WORKTREE, Color::Cyan),
    };
    draw::set(
        buf,
        area.x + 1,
        area.y,
        &shown(symbol),
        1,
        Style::new().fg(color),
    );
    let (name, detail) = parts(client, item);
    let name = shown(&name);
    let room = usize::from(area.width - 4);
    let name_room = if detail.is_empty() {
        room
    } else {
        room.min(NAME_MAX)
    };
    let name_width = columns(name.as_str()).min(name_room);
    draw::set_cut(buf, area.x + 3, area.y, &name, name_room, Style::new());
    let detail_x = area.x + 3 + u16::try_from(name_width + 2).unwrap_or(u16::MAX);
    let left = usize::from(area.right().saturating_sub(detail_x + 1));
    if left > 0 {
        let dim = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
        draw::set_cut(buf, detail_x, area.y, &shown(&detail), left, dim);
    }
}
