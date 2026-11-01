//! The session list on the left.
//!
//! The first three lines are the header: the search field (`⌕ Search…` across the sidebar), which opens the finder,
//! the workspace of the selected session's machine (`⌂ <name>`, or `⌂ choose a workspace`; [`Client::workspace`]),
//! which opens the switcher, then a `+ New session` button
//! (`⌕`, `⌂` and `+` when collapsed). Each session is two lines: its status glyph,
//! name and a `×` close button at the right edge, then `machine · PR`: the machine's name, from its host's hello
//! ([`Client::machine_label`]) in its color ([`crate::machine`]), then the PR's number and its checks (`#123 ✓`
//! passing, `✗` failing, `…` pending, nothing without checks), or its state when it isn't open (`#123 merged`), or
//! `—` without one. A session from another workspace is dimmed, and its second line says which workspace instead. The second line of a session that
//! failed to start is the reason, in red; of one whose worktree is being made (or its hooks run), `creating worktree…`;
//! of one that is closing while its worktree is removed, `removing worktree…`;
//! of one that started with a warning (a post-create hook failed), the warning, in yellow. A second line too long for
//! the row ends in `…`. The selected session has a highlighted background, and a session that wants the user has a
//! yellow bar on its left edge.
//! While there are peers (T31b), the list is grouped by machine ([`Client::sidebar`]): each group starts with a
//! two-line header, `≡` and the machine's name in its color, then how it is doing (`this machine`, `connected`,
//! `connecting…`, `offline: <why>`). A click on it means that machine ([`Target::Machine`]). A peer that can't be
//! reached now has its header and its last-known sessions dimmed. Collapsed, a header is just its `≡`.
//! The newest session is at the top (of each group). The list starts at the client's [`Client::sidebar_scroll`], which keeps the
//! selected session visible when the selection changes and follows the wheel otherwise. The last line is a footer:
//! the prefix and its bindings while it's pending; otherwise the latest notice across the whole width (one too long
//! for it is drawn across the window's last lines instead), else a reconnecting note or a hint for help with the
//! `Detach` and `?` buttons on the right, and a `Cleanup` button on the left while connected with a workspace on this
//! machine ([`Client::local_workspace`]; it then takes the hint's place).
//!
//! The third line also has `⊞ Layout` at its right, which opens the focused pane's menu.
//!
//! A session with subagents has a badge left of its `×`: their count, `▸N` with its tree collapsed, `▾N` expanded (cyan,
//! yellow while one is blocked on the user). A click on it expands or collapses the tree ([`Target::Subagents`]).
//! Expanded, each subagent has one line under its session: `├` (`└` for the last), its status glyph (`●` working, `▲`
//! blocked, `✓` finished), its agent type and its task, both cut to fit. A finished one is dimmed until the parent's
//! turn ends, when the fold drops it. A blocked subagent also turns its session's glyph `▲`, collapsed or not. A click
//! on a subagent's line means that subagent ([`Target::Subagent`]). The list counts these lines as entries too, so the
//! client's scroll covers them; [`capacity`] is how many entries are sure to fit (two-line ones), and as many more as
//! fit are drawn.
//!
//! Collapsed (narrow windows), only the `+`, the attention bar and glyph are drawn, with the `⊞` (layout), `⏏`
//! (detach) and `?` buttons on the last three lines; the footer shows `^B` instead while the prefix is pending. Rows have no `×` there:
//! a right click opens the row's menu, which has Close. Nor a badge: an expanded tree's lines are
//! the branch, in the attention column, and the subagent's glyph.
//!
//! [`crate::hit`] reads the same geometry ([`search_field`], [`workspace_button`], [`new_session_button`], [`visible_rows`],
//! [`close_button`], [`footer_buttons`]) that drawing uses.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Wrap};
use shepherd_core::acpp::{PrChecks, PrState};
use shepherd_core::client::{
    Client, ClientSession, Connection, Focus, MachineGroup, MachineStatus, SidebarEntry,
};
use shepherd_core::display::DisplayText;
use shepherd_core::session::{Activity, Subagent, SubagentStatus};

use crate::draw::{self, shown};
use crate::layout::{Layout, Target};
use crate::machine;
use crate::width::columns;

/// The most lines a notice too long for the footer takes at the bottom of the window.
const MAX_NOTICE_LINES: u16 = 3;

/// Lines per session (and per machine header).
const ROW_HEIGHT: u16 = 2;
/// Lines per subagent in an expanded tree.
const SUBAGENT_HEIGHT: u16 = 1;
/// Lines above the list: the search field, the workspace and `+ New session`.
const HEADER_HEIGHT: u16 = 3;
const SELECTED_BG: Color = Color::DarkGray;
const ATTENTION: Color = Color::Yellow;
const ATTENTION_BAR: &str = "▌";
/// What line 2 says of a session without a PR.
const NO_PR: &str = "—";
/// What line 2 says while a session's worktree is being made.
const CREATING_WORKTREE: &str = "creating worktree…";
/// What line 2 says while a closing session's worktree is being removed.
const REMOVING_WORKTREE: &str = "removing worktree…";

/// A session's PR as line 2 shows it: `#123 ✓`, `#123 merged`, or `—`.
fn pr_label(session: &ClientSession) -> String {
    let Some(pr) = &session.pr else {
        return NO_PR.to_owned();
    };
    let mark = match (&pr.state, &pr.checks) {
        (PrState::Open, PrChecks::Passing) => Some("✓"),
        (PrState::Open, PrChecks::Failing) => Some("✗"),
        (PrState::Open, PrChecks::Pending) => Some("…"),
        (PrState::Open, PrChecks::None | PrChecks::Other(_)) => None,
        (PrState::Merged, _) => Some("merged"),
        (PrState::Closed, _) => Some("closed"),
        (PrState::Other(state), _) => Some(state.as_str()),
    };
    match mark {
        Some(mark) => format!("#{} {mark}", pr.number),
        None => format!("#{}", pr.number),
    }
}

/// A session's status glyph and its colour. See the table in the crate docs.
pub fn glyph(session: &ClientSession) -> (&'static str, Color) {
    if session.exited {
        return ("✕", Color::Red);
    }
    if session.subagent_blocked() {
        return ("▲", Color::Yellow);
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

/// A subagent's status glyph and its colour.
fn subagent_glyph(subagent: &Subagent) -> (&'static str, Color) {
    match subagent.status() {
        SubagentStatus::Working => ("●", Color::Green),
        SubagentStatus::Blocked { .. } => ("▲", Color::Yellow),
        SubagentStatus::Finished => ("✓", Color::DarkGray),
    }
}

/// What a subagent's line calls it when the agent gave no type.
const UNNAMED_SUBAGENT: &str = "subagent";
/// How many columns a subagent's line gives its agent type, a blank column after it included.
const AGENT_TYPE_WIDTH: u16 = 9;

/// The badge of a session with subagents: `▸N` collapsed, `▾N` expanded. `None` without subagents.
fn badge_label(client: &Client, session: &ClientSession) -> Option<String> {
    let count = session.subagents.len();
    if count == 0 {
        return None;
    }
    let mark = match client.expanded(&session.id) {
        true => "▾",
        false => "▸",
    };
    Some(format!("{mark}{count}"))
}

/// Where the badge of `session`, drawn on `row`, is: left of the close button with a blank column between, starting
/// at least four columns before it. None in the collapsed sidebar, nor without subagents, nor where the name would get
/// no room.
pub(crate) fn subagent_badge(
    client: &Client,
    layout: &Layout,
    session: &ClientSession,
    row: Rect,
) -> Rect {
    let close = close_button(layout, row);
    let Some(label) = badge_label(client, session) else {
        return Rect::default();
    };
    if close.width == 0 {
        return Rect::default();
    }
    let width = u16::try_from(columns(&label)).unwrap_or(u16::MAX);
    let x = close
        .x
        .saturating_sub(4)
        .min(close.x.saturating_sub(1 + width));
    // The glyph, a blank, a column of name and a blank come first.
    if x < row.x + 5 {
        return Rect::default();
    }
    Rect::new(x, row.y, width, 1)
}

/// What marks a machine's header, in the glyph column.
const MACHINE_MARK: &str = "≡";
/// What this machine's header says of it while connected.
const THIS_MACHINE: &str = "this machine";

/// The search field's placeholder, and the collapsed sidebar's.
const SEARCH: &str = "⌕ Search…";
const SEARCH_COLLAPSED: &str = "⌕";

/// The button's label, and the collapsed sidebar's.
const NEW_SESSION: &str = "+ New session";
const NEW_SESSION_COLLAPSED: &str = "+";
/// The button that opens the focused pane's menu (split, close, move a session, pages), always shown so the layout can
/// be changed by click even when the pane's session takes the mouse (decision 0014).
const LAYOUT: &str = "⊞ Layout";
const LAYOUT_COLLAPSED: &str = "⊞";

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

/// Where the search field is: the first line, a column in from either side (its glyph alone when collapsed).
pub(crate) fn search_field(layout: &Layout) -> Rect {
    if layout.collapsed {
        return header_button(layout, 0, SEARCH_COLLAPSED);
    }
    let inner = inner(layout);
    if inner.width < 3 || inner.height == 0 {
        return Rect::default();
    }
    Rect::new(inner.x + 1, inner.y, inner.width - 2, 1)
}

/// Where the workspace button is: its label, on the second line.
pub(crate) fn workspace_button(client: &Client, layout: &Layout) -> Rect {
    header_button(layout, 1, workspace_label(client, layout).as_str())
}

/// Where the `+ New session` button is: its label, on the third line.
pub(crate) fn new_session_button(layout: &Layout) -> Rect {
    let label = if layout.collapsed {
        NEW_SESSION_COLLAPSED
    } else {
        NEW_SESSION
    };
    header_button(layout, 2, label)
}

/// Where the full sidebar's `⊞ Layout` button is: on the third line, right of `+ New session`, ending in the last column.
/// Collapsed, it is a footer button ([`footer_buttons`]).
pub(crate) fn layout_button(layout: &Layout) -> Rect {
    let inner = inner(layout);
    let width = u16::try_from(columns(LAYOUT)).unwrap_or(u16::MAX);
    if layout.collapsed || inner.height <= 2 || inner.width < width {
        return Rect::default();
    }
    let x = inner.right() - width;
    if x <= new_session_button(layout).right() {
        return Rect::default();
    }
    Rect::new(x, inner.y + 2, width, 1)
}

/// A row's close button, drawn as `×`.
const CLOSE: &str = "×";
const DETACH: &str = "Detach";
/// The footer.s button that reopens the restart dialog, while the host should be restarted.
const RESTART: &str = "Restart host";
/// The footer's button that shows the host's warning again, while it warns ([`Client::host_warning`]).
const HOST_WARNING: &str = "Host warning";
/// The footer's button that opens the cleanup view, while connected with a workspace on this machine (cleanup only ever
/// works on this machine's host, whichever session is selected).
const CLEANUP: &str = "Cleanup";
const HELP: &str = "?";
/// The collapsed sidebar's `Detach` button.
const DETACH_COLLAPSED: &str = "⏏";

/// Lines below the list: the footer, and in the collapsed sidebar the `⏏` line above it.
fn footer_height(layout: &Layout) -> u16 {
    if layout.collapsed { 3 } else { 1 }
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
/// full sidebar only), else `Host warning` while the host warns ([`Client::host_warning`]), else `Cleanup` while
/// connected with a workspace (full sidebar only), `Detach`, then `?` in the last column. Collapsed, they are glyphs in the
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
        let mut buttons = vec![
            (detach, Target::Detach, DETACH_COLLAPSED),
            (help, Target::Help, HELP),
        ];
        if inner.height >= HEADER_HEIGHT + 3 {
            let at = Rect {
                y: detach.y - 1,
                ..detach
            };
            buttons.insert(0, (at, Target::LayoutMenu, LAYOUT_COLLAPSED));
        }
        return buttons;
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
    } else if *client.connection() == Connection::Ready && client.local_workspace().is_some() {
        Some((Target::Cleanup, CLEANUP))
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

/// How many lines `entry` takes.
fn height(entry: &SidebarEntry) -> u16 {
    match entry {
        SidebarEntry::Machine(_) | SidebarEntry::Session(_) => ROW_HEIGHT,
        SidebarEntry::Subagent { .. } => SUBAGENT_HEIGHT,
    }
}

/// Each entry shown (a machine's header, a session or a subagent) and its lines, top to bottom, from the client's
/// scroll: as many as fit whole. When the rest of the list fits with room to spare, entries above the scroll fill
/// that room, so one-line subagents never leave the list's top hidden for nothing.
pub(crate) fn visible_rows<'a>(
    client: &'a Client,
    layout: &Layout,
) -> Vec<(SidebarEntry<'a>, Rect)> {
    let list = list(layout);
    let visible = capacity(layout);
    if visible == 0 {
        return Vec::new();
    }
    let entries = client.sidebar();
    let mut first = client
        .sidebar_scroll()
        .min(entries.len().saturating_sub(visible));
    let mut tail: usize = entries[first..]
        .iter()
        .map(|e| usize::from(height(e)))
        .sum();
    while let Some(above) = first.checked_sub(1) {
        let taller = tail + usize::from(height(&entries[above]));
        if taller > usize::from(list.height) {
            break;
        }
        tail = taller;
        first = above;
    }
    let mut y = list.y;
    let mut rows = Vec::new();
    for entry in entries.into_iter().skip(first) {
        let height = height(&entry);
        if y + height > list.bottom() {
            break;
        }
        rows.push((entry, Rect::new(list.x, y, list.width, height)));
        y += height;
    }
    rows
}

/// What a machine header's second line says.
pub(crate) fn machine_status(group: &MachineGroup) -> String {
    match (&group.status, group.local) {
        (MachineStatus::Connected, true) => THIS_MACHINE.to_owned(),
        (MachineStatus::Connected, false) => "connected".to_owned(),
        (MachineStatus::Connecting, _) => "connecting…".to_owned(),
        (MachineStatus::Offline(Some(why)), _) => format!("offline: {why}"),
        (MachineStatus::Offline(None), _) => "offline".to_owned(),
        (MachineStatus::Incompatible, _) => "another protocol version".to_owned(),
    }
}

/// Whether a machine's group is drawn dimmed: a peer that isn't connected, whose sessions are as last known.
fn dimmed(group: &MachineGroup) -> bool {
    !group.local && group.status != MachineStatus::Connected
}

/// Draws a machine's header on its two `line`s: its mark and name, then how it is doing (just the mark when
/// collapsed).
fn render_machine(
    client: &Client,
    group: &MachineGroup,
    layout: &Layout,
    line: Rect,
    buf: &mut Buffer,
) {
    let dim = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    let mark = match dimmed(group) {
        true => dim,
        false => Style::new().fg(Color::Gray),
    };
    draw::set(
        buf,
        line.x + 1,
        line.y,
        &shown(MACHINE_MARK),
        usize::MAX,
        mark,
    );
    if layout.collapsed {
        return;
    }
    let width = usize::from(line.width.saturating_sub(3));
    let mut name = machine::style(client, &group.id).add_modifier(Modifier::BOLD);
    if dimmed(group) {
        name = name.add_modifier(Modifier::DIM);
    }
    draw::set_cut(buf, line.x + 3, line.y, &shown(&group.name), width, name);
    let status = shown(&machine_status(group));
    draw::set_cut(buf, line.x + 3, line.y + 1, &status, width, dim);
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
    let field = search_field(layout);
    let label = if layout.collapsed {
        SEARCH_COLLAPSED
    } else {
        SEARCH
    };
    let placeholder = Style::new().fg(Color::Gray);
    let width = usize::from(field.width);
    draw::set(buf, field.x, field.y, &shown(label), width, placeholder);
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
    let button = layout_button(layout);
    if button.width > 0 {
        let width = usize::from(button.width);
        draw::set(buf, button.x, button.y, &shown(LAYOUT), width, style);
    }
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
        let offline = client.offline(&session.id);
        if session.elsewhere || offline {
            style = style.fg(Color::Gray).add_modifier(Modifier::DIM);
        }
        let (symbol, color) = glyph(session);
        let mut glyph_style = Style::new().fg(color);
        if offline {
            glyph_style = glyph_style.add_modifier(Modifier::DIM);
        }
        draw::set(
            buf,
            line.x + 1,
            line.y,
            &shown(symbol),
            usize::MAX,
            glyph_style,
        );
        // The name stops a column short of the badge, or of the close button.
        let badge = subagent_badge(client, layout, session, line);
        let name_width = match badge.width {
            0 => usize::from(line.width.saturating_sub(5)),
            _ => usize::from(badge.x.saturating_sub(line.x + 4)),
        };
        let name = shown(&session.name);
        draw::set(buf, line.x + 3, line.y, &name, name_width, style);
        if let Some(label) = badge_label(client, session).filter(|_| badge.width > 0) {
            let color = match session.subagent_blocked() {
                true => Color::Yellow,
                false => Color::Cyan,
            };
            let style = Style::new().fg(color).add_modifier(Modifier::BOLD);
            let width = usize::from(badge.width);
            draw::set(buf, badge.x, badge.y, &shown(&label), width, style);
        }
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
        let label = shown(client.machine_label(&session.machine));
        let (text, style, named) = match (&session.failed, &session.warning, &session.workspace) {
            _ if session.removing_worktree => {
                (REMOVING_WORKTREE.to_owned(), dim.fg(Color::Gray), false)
            }
            (Some(reason), _, _) => (reason.clone(), dim.fg(Color::Red), false),
            _ if session.creating_worktree => {
                (CREATING_WORKTREE.to_owned(), dim.fg(Color::Gray), false)
            }
            (None, Some(warning), _) => (warning.clone(), dim.fg(Color::Yellow), false),
            (None, None, Some(workspace)) if session.elsewhere => (
                format!("in {}", workspace_name(workspace)),
                dim.fg(Color::Gray),
                false,
            ),
            _ => (
                format!("{} · {}", label.as_str(), pr_label(session)),
                dim.fg(Color::Gray),
                true,
            ),
        };
        let text = shown(&text);
        draw::set_cut(buf, line.x + 3, line.y + 1, &text, width, style);
        if named {
            // Cut short, the name keeps its color up to the `…`.
            let room = match columns(text.as_str()) <= width {
                true => width,
                false => width.saturating_sub(1),
            };
            let at = (line.x + 3, line.y + 1);
            machine::tint(client, &session.machine, label.as_str(), at, room, buf);
        }
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
    for (entry, line) in visible_rows(client, layout) {
        let session = match entry {
            SidebarEntry::Session(session) => session,
            SidebarEntry::Machine(group) => {
                render_machine(client, &group, layout, line, buf);
                continue;
            }
            SidebarEntry::Subagent { subagent, last, .. } => {
                render_subagent(subagent, last, layout, line, buf);
                continue;
            }
        };
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

/// Draws a subagent's line: its branch, glyph, agent type and task (the branch and glyph alone when collapsed).
fn render_subagent(subagent: &Subagent, last: bool, layout: &Layout, line: Rect, buf: &mut Buffer) {
    let branch = shown(if last { "└" } else { "├" });
    let branch_style = Style::new().fg(Color::DarkGray);
    let (symbol, color) = subagent_glyph(subagent);
    let glyph_style = Style::new().fg(color);
    if layout.collapsed {
        draw::set(buf, line.x, line.y, &branch, 1, branch_style);
        draw::set(buf, line.x + 1, line.y, &shown(symbol), 1, glyph_style);
        return;
    }
    // Everything stops a column short of the border.
    let room = |x: u16| usize::from(line.right().saturating_sub(x + 1));
    draw::set(
        buf,
        line.x + 3,
        line.y,
        &branch,
        room(line.x + 3),
        branch_style,
    );
    draw::set(
        buf,
        line.x + 5,
        line.y,
        &shown(symbol),
        room(line.x + 5),
        glyph_style,
    );
    let (kind_style, task_style) = match subagent.status() {
        SubagentStatus::Finished => {
            let dim = Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM);
            (dim, dim)
        }
        _ => (
            Style::new().fg(Color::Magenta),
            Style::new().fg(Color::Gray),
        ),
    };
    let kind = match subagent.agent_type() {
        "" => UNNAMED_SUBAGENT,
        kind => kind,
    };
    let x = line.x + 7;
    let kind_width = usize::from(AGENT_TYPE_WIDTH - 1).min(room(x));
    draw::set_cut(buf, x, line.y, &shown(kind), kind_width, kind_style);
    if let Some(task) = subagent.task() {
        let x = x + AGENT_TYPE_WIDTH;
        draw::set_cut(buf, x, line.y, &shown(task), room(x), task_style);
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
    draw::render(paragraph.clone(), scratch.area, &mut scratch);
    let used = (0..MAX_NOTICE_LINES)
        .rev()
        .find(|&y| (0..width).any(|x| scratch[(x, y)].symbol() != " "))
        .map_or(1, |y| y + 1);
    let height = used.min(window.height);
    let area = Rect::new(window.x, window.bottom() - height, window.width, height);
    draw::render(Clear, area, buf);
    draw::render(paragraph, Rect::new(area.x + 1, area.y, width, height), buf);
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
