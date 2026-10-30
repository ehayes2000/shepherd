//! The session list on the left.
//!
//! The first two lines are the header: the workspace (`⌂ <name>`, or `⌂ choose a workspace`), which opens the
//! switcher, then a `+ New session` button (`⌂` and `+` when collapsed). Each session is two lines: its status glyph,
//! name and a `×` close button at the right edge, then `machine · PR` (placeholders until M3 / M4). A session from
//! another workspace is dimmed, and its second line says which workspace instead. The second line of a session that
//! failed to start is the reason, in red; of one whose worktree is being made (or its hooks run), `creating worktree…`;
//! of one that is closing while its worktree is removed, `removing worktree…`;
//! of one that started with a warning (a post-create hook failed), the warning, in yellow. A second line too long for
//! the row ends in `…`. The selected session has a highlighted background, and a session that wants the user has a
//! yellow bar on its left edge.
//! The newest session is at the top. The list starts at the client's [`Client::sidebar_scroll`], which keeps the
//! selected session visible when the selection changes and follows the wheel otherwise. The last line is a footer:
//! the prefix and its bindings while it's pending; otherwise the latest notice across the whole width (one too long
//! for it is drawn across the window's last lines instead), else a reconnecting note or a hint for help with the
//! `Detach` and `?` buttons on the right.
//!
//! Collapsed (narrow windows), only the `+`, the attention bar and glyph are drawn, with the `⏏` (detach) and `?`
//! buttons on the last two lines; the footer shows `^B` instead while the prefix is pending. Rows have no `×` there:
//! a right click opens the row's menu, which has Close.
//!
//! [`crate::hit`] reads the same geometry ([`workspace_button`], [`new_session_button`], [`visible_rows`],
//! [`close_button`], [`footer_buttons`]) that drawing uses.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget, Wrap};
use shepherd_core::client::{Client, ClientSession, Connection, Focus};
use shepherd_core::display::DisplayText;
use shepherd_core::session::Activity;

use crate::draw::{self, shown};
use crate::layout::{Layout, Target};
use crate::width::columns;

/// The most lines a notice too long for the footer takes at the bottom of the window.
const MAX_NOTICE_LINES: u16 = 3;

/// Lines per session.
const ROW_HEIGHT: u16 = 2;
/// Lines above the list: the workspace and `+ New session`.
const HEADER_HEIGHT: u16 = 2;
const SELECTED_BG: Color = Color::DarkGray;
const ATTENTION: Color = Color::Yellow;
const ATTENTION_BAR: &str = "▌";
/// What line 2 says until sessions know their machine (M4) and PR (M3).
const PLACEHOLDER_DETAIL: &str = "local · —";
/// What line 2 says while a session's worktree is being made.
const CREATING_WORKTREE: &str = "creating worktree…";
/// What line 2 says while a closing session's worktree is being removed.
const REMOVING_WORKTREE: &str = "removing worktree…";

/// A session's status glyph and its colour. See the table in the crate docs.
pub fn glyph(session: &ClientSession) -> (&'static str, Color) {
    if session.exited {
        return ("✕", Color::Red);
    }
    match session.activity {
        Activity::Starting => ("◌", Color::DarkGray),
        Activity::Working { .. } => ("●", Color::Green),
        Activity::NeedsPermission { .. } | Activity::NeedsInput => ("▲", Color::Yellow),
        Activity::Idle => ("○", Color::Reset),
        Activity::Exited { .. } => ("✕", Color::Red),
        // Not `?`, which is the help button in the collapsed sidebar's glyph column.
        Activity::Unknown => ("·", Color::DarkGray),
    }
}

/// The button's label, and the collapsed sidebar's.
const NEW_SESSION: &str = "+ New session";
const NEW_SESSION_COLLAPSED: &str = "+";

/// The sidebar left of its border.
fn inner(layout: &Layout) -> Rect {
    let area = layout.sidebar;
    Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height)
}

/// The workspace's label: its directory's name, or an offer to choose one.
const WORKSPACE_MARK: &str = "⌂";
const NO_WORKSPACE: &str = "⌂ choose a workspace";

fn workspace_label(client: &Client, layout: &Layout) -> DisplayText {
    if layout.collapsed {
        return shown(WORKSPACE_MARK);
    }
    match client.workspace() {
        Some(path) => shown(&format!("{WORKSPACE_MARK} {}", workspace_name(path))),
        None => shown(NO_WORKSPACE),
    }
}

/// What a workspace is called in the sidebar: its directory's name.
fn workspace_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// A label on header line `line`, a column in from the left, cut to fit.
fn header_button(layout: &Layout, line: u16, label: &str) -> Rect {
    let inner = inner(layout);
    if inner.width < 2 || inner.height <= line {
        return Rect::default();
    }
    let width = u16::try_from(columns(label))
        .unwrap_or(u16::MAX)
        .min(inner.width - 1);
    Rect::new(inner.x + 1, inner.y + line, width, 1)
}

/// Where the workspace button is: its label, on the first line.
pub(crate) fn workspace_button(client: &Client, layout: &Layout) -> Rect {
    header_button(layout, 0, workspace_label(client, layout).as_str())
}

/// Where the `+ New session` button is: its label, on the second line.
pub(crate) fn new_session_button(layout: &Layout) -> Rect {
    let label = if layout.collapsed {
        NEW_SESSION_COLLAPSED
    } else {
        NEW_SESSION
    };
    header_button(layout, 1, label)
}

/// A row's close button, drawn as `×`.
const CLOSE: &str = "×";
const DETACH: &str = "Detach";
/// The footer.s button that reopens the restart dialog, while the host should be restarted.
const RESTART: &str = "Restart host";
/// The footer's button that shows the host's warning again, while it warns ([`Client::host_warning`]).
const HOST_WARNING: &str = "Host warning";
const HELP: &str = "?";
/// The collapsed sidebar's `Detach` button.
const DETACH_COLLAPSED: &str = "⏏";

/// Lines below the list: the footer, and in the collapsed sidebar the `⏏` line above it.
fn footer_height(layout: &Layout) -> u16 {
    if layout.collapsed { 2 } else { 1 }
}

/// Where the close button of the session drawn on `row` is: the last cell of its first line. The collapsed sidebar
/// has none.
pub(crate) fn close_button(layout: &Layout, row: Rect) -> Rect {
    if layout.collapsed || row.width < 6 || row.height == 0 {
        return Rect::default();
    }
    Rect::new(row.right() - 1, row.y, 1, 1)
}

/// The footer's buttons, left to right: `Restart host` while the host should be restarted ([`Client::restart_offer`],
/// full sidebar only), else `Host warning` while the host warns ([`Client::host_warning`]), `Detach`, then `?` in the
/// last column. Collapsed, they are glyphs in the
/// glyph column: `⏏` just above `?` on the last line. Not shown while the prefix is pending, when the footer shows
/// it instead, nor in the full sidebar while a notice shows, which then has the whole footer.
pub(crate) fn footer_buttons(
    client: &Client,
    layout: &Layout,
) -> Vec<(Rect, Target, &'static str)> {
    let inner = inner(layout);
    if client.focus() == Focus::Command {
        return Vec::new();
    }
    if !layout.collapsed && client.notice().is_some() {
        return Vec::new();
    }
    if layout.collapsed {
        if inner.height < HEADER_HEIGHT + 2 || inner.width < 2 {
            return Vec::new();
        }
        let help = Rect::new(inner.x + 1, inner.bottom() - 1, 1, 1);
        let detach = Rect {
            y: help.y - 1,
            ..help
        };
        return vec![
            (detach, Target::Detach, DETACH_COLLAPSED),
            (help, Target::Help, HELP),
        ];
    }
    let width = u16::try_from(columns(DETACH)).unwrap_or(u16::MAX);
    if inner.height < HEADER_HEIGHT + 1 || inner.width < width + 6 {
        return Vec::new();
    }
    let y = inner.bottom() - 1;
    let help = Rect::new(inner.right() - 1, y, 1, 1);
    let detach = Rect::new(help.x - 1 - width, y, width, 1);
    let mut buttons = vec![(detach, Target::Detach, DETACH), (help, Target::Help, HELP)];
    let first = if client.restart_offer().is_some() {
        Some((Target::OfferRestart, RESTART))
    } else if client.host_warning().is_some() {
        Some((Target::ShowHostWarning, HOST_WARNING))
    } else {
        None
    };
    if let Some((target, label)) = first {
        let width = u16::try_from(columns(label)).unwrap_or(u16::MAX);
        // A blank column between it and `Detach`.
        if detach.x > inner.x + 1 + width {
            let at = Rect::new(inner.x + 1, y, width, 1);
            buttons.insert(0, (at, target, label));
        }
    }
    buttons
}

/// The session rows, between the header and the footer.
fn list(layout: &Layout) -> Rect {
    let inner = inner(layout);
    Rect::new(
        inner.x,
        inner.y.saturating_add(HEADER_HEIGHT),
        inner.width,
        inner
            .height
            .saturating_sub(HEADER_HEIGHT + footer_height(layout)),
    )
}

/// How many sessions the sidebar of `layout` shows at once.
pub(crate) fn capacity(layout: &Layout) -> usize {
    let list = list(layout);
    if list.width < 2 {
        return 0;
    }
    usize::from(list.height / ROW_HEIGHT)
}

/// Each session shown, as its index in the client's list and its two lines, top to bottom, from the client's scroll.
pub(crate) fn visible_rows(
    client: &Client,
    layout: &Layout,
) -> impl Iterator<Item = (usize, Rect)> {
    let list = list(layout);
    let visible = capacity(layout);
    let len = client.sessions().len();
    let first = client.sidebar_scroll().min(len.saturating_sub(visible));
    (first..len.min(first + visible))
        .zip(0u16..)
        .map(move |(index, slot)| {
            let y = list.y + slot * ROW_HEIGHT;
            (index, Rect::new(list.x, y, list.width, ROW_HEIGHT))
        })
}

pub(crate) fn render(client: &Client, layout: &Layout, buf: &mut Buffer) {
    let area = layout.sidebar;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = Style::new().fg(Color::DarkGray);
    for y in area.top()..area.bottom() {
        draw::set(buf, area.right() - 1, y, &shown("│"), 1, border);
    }
    let workspace = workspace_button(client, layout);
    let style = match client.workspace() {
        Some(_) => Style::new().add_modifier(Modifier::BOLD),
        None => Style::new().fg(Color::Gray),
    };
    let label = workspace_label(client, layout);
    draw::set(
        buf,
        workspace.x,
        workspace.y,
        &label,
        usize::from(workspace.width),
        style,
    );
    let button = new_session_button(layout);
    let label = if layout.collapsed {
        NEW_SESSION_COLLAPSED
    } else {
        NEW_SESSION
    };
    let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let width = usize::from(button.width);
    draw::set(buf, button.x, button.y, &shown(label), width, style);
    if layout.collapsed {
        render_collapsed(client, layout, buf);
    } else {
        render_full(client, layout, buf);
    }
}

fn render_full(client: &Client, layout: &Layout, buf: &mut Buffer) {
    rows(client, layout, buf, |session, selected, line, buf| {
        let width = usize::from(line.width.saturating_sub(3));
        let mut style = Style::new();
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        if session.elsewhere {
            style = style.fg(Color::Gray).add_modifier(Modifier::DIM);
        }
        let (symbol, color) = glyph(session);
        let glyph_style = Style::new().fg(color);
        draw::set(
            buf,
            line.x + 1,
            line.y,
            &shown(symbol),
            usize::MAX,
            glyph_style,
        );
        // The name stops a column short of the close button.
        let name_width = usize::from(line.width.saturating_sub(5));
        let name = shown(&session.name);
        draw::set(buf, line.x + 3, line.y, &name, name_width, style);
        let close = close_button(layout, line);
        draw::set(
            buf,
            close.x,
            close.y,
            &shown(CLOSE),
            usize::from(close.width),
            Style::new().fg(Color::Gray),
        );
        let dim = Style::new().add_modifier(Modifier::DIM);
        let (text, style) = match (&session.failed, &session.warning, &session.workspace) {
            _ if session.removing_worktree => (REMOVING_WORKTREE.to_owned(), dim.fg(Color::Gray)),
            (Some(reason), _, _) => (reason.clone(), dim.fg(Color::Red)),
            _ if session.creating_worktree => (CREATING_WORKTREE.to_owned(), dim.fg(Color::Gray)),
            (None, Some(warning), _) => (warning.clone(), dim.fg(Color::Yellow)),
            (None, None, Some(workspace)) if session.elsewhere => (
                format!("in {}", workspace_name(workspace)),
                dim.fg(Color::Gray),
            ),
            _ => (PLACEHOLDER_DETAIL.to_owned(), dim.fg(Color::Gray)),
        };
        draw::set_cut(buf, line.x + 3, line.y + 1, &shown(&text), width, style);
    });
    let area = inner(layout);
    if area.height > HEADER_HEIGHT {
        let buttons = footer_buttons(client, layout);
        let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        for (at, _, label) in &buttons {
            draw::set(
                buf,
                at.x,
                at.y,
                &shown(label),
                usize::from(at.width),
                button,
            );
        }
        // The status keeps a blank column before the buttons.
        let line = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let status = match buttons.first() {
            Some((detach, ..)) => Rect {
                width: detach.x.saturating_sub(line.x + 1),
                ..line
            },
            None => line,
        };
        footer(client, status, buf);
    }
}

fn render_collapsed(client: &Client, layout: &Layout, buf: &mut Buffer) {
    rows(client, layout, buf, |session, _, line, buf| {
        let (symbol, color) = glyph(session);
        let glyph_style = Style::new().fg(color);
        draw::set(
            buf,
            line.x + 1,
            line.y,
            &shown(symbol),
            usize::MAX,
            glyph_style,
        );
    });
    let button = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    for (at, _, label) in footer_buttons(client, layout) {
        draw::set(
            buf,
            at.x,
            at.y,
            &shown(label),
            usize::from(at.width),
            button,
        );
    }
    let area = inner(layout);
    if client.focus() == Focus::Command && area.height > HEADER_HEIGHT {
        let style = Style::new().add_modifier(Modifier::REVERSED);
        draw::set(
            buf,
            area.x,
            area.bottom() - 1,
            &shown("^B"),
            usize::from(area.width),
            style,
        );
    }
}

/// Draws each visible session's two lines with `draw`, after the selection highlight and attention bar.
fn rows(
    client: &Client,
    layout: &Layout,
    buf: &mut Buffer,
    mut draw: impl FnMut(&ClientSession, bool, Rect, &mut Buffer),
) {
    let sessions = client.sessions();
    for (index, line) in visible_rows(client, layout) {
        let session = &sessions[index];
        let is_selected = client.selected() == Some(&session.id);
        if is_selected {
            buf.set_style(line, Style::new().bg(SELECTED_BG));
        }
        if session.attention {
            for dy in 0..ROW_HEIGHT {
                draw::set(
                    buf,
                    line.x,
                    line.y + dy,
                    &shown(ATTENTION_BAR),
                    usize::MAX,
                    Style::new().fg(ATTENTION),
                );
            }
        }
        draw(session, is_selected, line, buf);
    }
}

/// A notice too long for the full sidebar's footer: drawn across the window's last lines instead (as many as it
/// takes, up to [`MAX_NOTICE_LINES`]), over the footer and the pane. Not while the prefix is pending (the footer
/// shows that), nor beside the collapsed sidebar, which keeps its buttons and shows no notice.
pub(crate) fn wide_notice(client: &Client, layout: &Layout, buf: &mut Buffer) {
    let Some(notice) = client.notice().map(shown) else {
        return;
    };
    let window = layout.sidebar.union(layout.pane);
    if client.focus() == Focus::Command || window.width < 2 || window.height == 0 {
        return;
    }
    let room = usize::from(inner(layout).width.saturating_sub(1));
    if layout.collapsed || columns(notice.as_str()) <= room {
        return;
    }
    // A blank column on the left, like the footer.
    let width = window.width - 1;
    let paragraph = draw::paragraph(vec![draw::line(notice, Style::new().fg(ATTENTION))])
        .wrap(Wrap { trim: true });
    // As many lines as the wrapped text takes: wrapped on a scratch buffer first, so word breaks count.
    let mut scratch = Buffer::empty(Rect::new(0, 0, width, MAX_NOTICE_LINES));
    paragraph.clone().render(scratch.area, &mut scratch);
    let used = (0..MAX_NOTICE_LINES)
        .rev()
        .find(|&y| (0..width).any(|x| scratch[(x, y)].symbol() != " "))
        .map_or(1, |y| y + 1);
    let height = used.min(window.height);
    let area = Rect::new(window.x, window.bottom() - height, window.width, height);
    Clear.render(area, buf);
    paragraph.render(Rect::new(area.x + 1, area.y, width, height), buf);
}

fn footer(client: &Client, area: Rect, buf: &mut Buffer) {
    let width = usize::from(area.width.saturating_sub(1));
    let (x, y) = (area.x + 1, area.y);
    if client.focus() == Focus::Command {
        let (end, _) = draw::set(
            buf,
            x,
            y,
            &shown("ctrl-b"),
            width,
            Style::new().add_modifier(Modifier::REVERSED),
        );
        let rest = width.saturating_sub(usize::from(end - x));
        let keys = shown(" n j k r x ? q");
        draw::set(buf, end, y, &keys, rest, Style::new().fg(Color::Gray));
    } else if let Some(notice) = client.notice() {
        let notice = shown(notice);
        draw::set(buf, x, y, &notice, width, Style::new().fg(ATTENTION));
    } else if *client.connection() != Connection::Ready && !client.sessions().is_empty() {
        let note = shown("reconnecting…");
        draw::set(buf, x, y, &note, width, Style::new().fg(ATTENTION));
    } else {
        let hint = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
        draw::set(buf, x, y, &shown("ctrl-b ? help"), width, hint);
    }
}
