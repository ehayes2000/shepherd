//! The status page (T57): a pane showing [`ViewKind::Status`](shepherd_core::layout::ViewKind::Status) draws every
//! session on every machine in one table, from the client's [`Client::status_lines`].
//!
//! - The first line counts each group (`▲ 3 need you`, `● 5 working`, `○ 2 idle`, `✕ 1 exited`), the subagents and the
//!   machines ([`Client::status_counts`]).
//! - Below it, a box titled `agents` holds the table: a line of column names, then the table's lines from
//!   [`Client::status_scroll`]. Its top edge has the grouping toggle at its right, `[g: by machine]` or
//!   `[g: by state]`.
//! - Each session's row: its status glyph, name, agent, machine (in its color, [`crate::machine`]), subagent count
//!   (`↳N`, yellow while one waits on the user), what it is doing (what it asks, its tool, or how it is), and its PR
//!   with its checks. Columns that don't fit are left out, the subagents first, then the agent, the PR and the machine.
//!   The table's selected row ([`Client::status_selected`]) is highlighted; a session on a machine that can't be
//!   reached now is dimmed, and says it is offline.
//! - Grouped by state, each group has a header (`NEEDS YOU  3`); grouped by machine, each machine (its name, how it is
//!   doing and its count).
//!
//! The table takes the whole pane for now; the preview (T58) and the events feed and message line (T59) will take its
//! right part and bottom lines.
//!
//! [`hit`] reads the same geometry: a session's row ([`Target::StatusRow`]) and the toggle ([`Target::StatusGroupBy`]).

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;
use shepherd_core::acpp::{PrChecks, PrState};
use shepherd_core::client::panes::{Area, arrange};
use shepherd_core::client::status::{Group, Grouping, StatusCounts, StatusLine};
use shepherd_core::client::{Client, ClientSession, MachineGroup, MachineStatus};
use shepherd_core::config::AgentKind;
use shepherd_core::layout::{Content, Pane, PaneId, ViewKind};
use shepherd_core::session::{Activity, SubagentStatus};

use crate::draw::{self, shown};
use crate::layout::Target;
use crate::sidebar::{glyph, machine_status, pr_label};
use crate::width::{columns, cut};
use crate::{machine, panes};

const SELECTED_BG: Color = Color::DarkGray;
const TITLE: &str = "agents";
/// What a cell with nothing to say shows.
const NONE: &str = "—";

/// Where the page's parts are in its area.
struct Geometry {
    /// The line of counts.
    header: Rect,
    /// The table's box, when there is room for one.
    table: Option<Table>,
}

struct Table {
    frame: Rect,
    /// The line of column names.
    names: Rect,
    /// The table's lines.
    lines: Rect,
}

fn geometry(area: Rect) -> Geometry {
    let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let table = (area.height >= 4 && area.width >= 12).then(|| {
        let frame = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        let inner = Rect::new(frame.x + 1, frame.y + 1, frame.width - 2, frame.height - 2);
        Table {
            frame,
            names: Rect::new(inner.x, inner.y, inner.width, 1),
            lines: Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
        }
    });
    Geometry { header, table }
}

/// How many of the table's lines a status page in `area` shows at once.
fn shown_in(area: Rect) -> usize {
    geometry(area)
        .table
        .map_or(0, |t| usize::from(t.lines.height))
}

/// The toggle's label: what it switches to.
fn toggle_label(grouping: Grouping) -> &'static str {
    match grouping {
        Grouping::State => "[g: by machine]",
        Grouping::Machine => "[g: by state]",
    }
}

/// Where the toggle is: at the right of the box's top edge, a blank and a corner after it, if it fits after the title.
fn toggle(client: &Client, table: &Table) -> Option<Rect> {
    let width = u16::try_from(columns(toggle_label(client.status_grouping()))).ok()?;
    let title_end = table.frame.x + 2 + u16::try_from(columns(TITLE)).unwrap_or(0) + 2;
    let x = table.frame.right().checked_sub(width + 2)?;
    (x >= title_end).then(|| Rect::new(x, table.frame.y, width, 1))
}

/// The first line the table shows: the client's scroll, no further than the last lines fill it.
fn first_line(client: &Client, len: usize, shown: usize) -> usize {
    client.status_scroll().min(len.saturating_sub(shown))
}

/// Each table line shown, with the screen line it is on.
fn visible<'a>(client: &'a Client, table: &Table) -> Vec<(StatusLine<'a>, Rect)> {
    let lines = client.status_lines();
    let shown = usize::from(table.lines.height);
    let first = first_line(client, lines.len(), shown);
    lines
        .into_iter()
        .skip(first)
        .take(shown)
        .zip(table.lines.rows())
        .collect()
}

/// What is under cell `col`, `row` of a status page drawn in pane `pane` at `area`.
pub(crate) fn hit(client: &Client, pane: PaneId, area: Rect, col: u16, row: u16) -> Option<Target> {
    let at = Position::new(col, row);
    let table = geometry(area).table?;
    let shown = usize::from(table.lines.height);
    if toggle(client, &table).is_some_and(|t| t.contains(at)) {
        return Some(Target::StatusGroupBy {
            pane,
            by: client.status_grouping().other(),
            shown,
        });
    }
    visible(client, &table)
        .into_iter()
        .find(|(_, line)| line.contains(at))
        .and_then(|(line, _)| match line {
            StatusLine::Session(s) => Some(Target::StatusRow {
                pane,
                session: s.id.clone(),
                shown,
            }),
            _ => None,
        })
}

/// Where the status page shown in the focused pane is drawn, if the focused pane shows it: in the view the client
/// arranges its layout in ([`Client::size`]), which has the pane area's size.
fn focused_area(client: &Client) -> Option<Rect> {
    let layout = client.host_layout();
    let focused = client.focused_pane();
    let Some(Pane::Leaf {
        content: Content::View(ViewKind::Status),
        ..
    }) = layout.pane(focused)
    else {
        return None;
    };
    let size = client.size()?;
    let view = Area {
        x: 0,
        y: 0,
        cols: size.cols,
        rows: size.rows,
    };
    let arranged = arrange(&layout, view);
    arranged.pane(focused)?.terminal().map(panes::rect)
}

/// Whether the focused pane shows the status page: keys go to its table.
pub(crate) fn focused(client: &Client) -> bool {
    let layout = client.host_layout();
    matches!(
        layout.pane(client.focused_pane()),
        Some(Pane::Leaf {
            content: Content::View(ViewKind::Status),
            ..
        })
    )
}

/// How many lines the focused pane's status table shows at once.
pub(crate) fn focused_shown(client: &Client) -> usize {
    focused_area(client).map_or(0, shown_in)
}

/// The status page drawn in pane `pane` at `area` under cell `col`, `row` of the pane area `pane_area`, with how many
/// lines its table shows: for the wheel.
pub(crate) fn under(client: &Client, pane_area: Rect, col: u16, row: u16) -> Option<usize> {
    let arranged = panes::arranged(client, pane_area);
    arranged
        .panes
        .iter()
        .filter(|p| p.content == Content::View(ViewKind::Status))
        .filter_map(|p| p.terminal().map(panes::rect))
        .find(|area| area.contains(Position::new(col, row)))
        .map(shown_in)
}

// ---------- drawing ----------

/// Draws the status page in `area`.
pub(crate) fn render(client: &Client, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let geometry = geometry(area);
    render_counts(&client.status_counts(), geometry.header, buf);
    let Some(table) = geometry.table else {
        return;
    };
    let border = Style::new().fg(Color::DarkGray);
    draw::render(Block::bordered().border_style(border), table.frame, buf);
    let title = shown(&format!(" {TITLE} "));
    let title_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let room = usize::from(table.frame.width.saturating_sub(3));
    draw::set(
        buf,
        table.frame.x + 1,
        table.frame.y,
        &title,
        room,
        title_style,
    );
    if let Some(t) = toggle(client, &table) {
        let label = shown(toggle_label(client.status_grouping()));
        draw::set(
            buf,
            t.x,
            t.y,
            &label,
            usize::from(t.width),
            Style::new().fg(Color::Gray),
        );
    }
    let cols = Columns::fit(table.lines.width);
    render_names(&cols, table.names, buf);
    let selected = client.status_selected();
    for (line, at) in visible(client, &table) {
        match line {
            StatusLine::Group { group, count } => render_group(group, count, at, buf),
            StatusLine::Machine { machine, count } => {
                render_machine(client, &machine, count, at, buf)
            }
            StatusLine::Session(session) => {
                let is_selected = selected == Some(&session.id);
                render_row(client, session, is_selected, &cols, at, buf);
            }
            StatusLine::Gap => {}
        }
    }
}

fn render_counts(counts: &StatusCounts, line: Rect, buf: &mut Buffer) {
    if line.height == 0 {
        return;
    }
    let gap = || (shown("   "), Style::new());
    let machines = match counts.machines {
        1 => "1 machine".to_owned(),
        n => format!("{n} machines"),
    };
    let subagents = match counts.subagents {
        1 => "↳ 1 subagent".to_owned(),
        n => format!("↳ {n} subagents"),
    };
    let parts = vec![
        (
            shown(&format!("▲ {} need you", counts.needs_you)),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        gap(),
        (
            shown(&format!("● {} working", counts.working)),
            Style::new().fg(Color::Green),
        ),
        gap(),
        (shown(&format!("○ {} idle", counts.idle)), Style::new()),
        gap(),
        (
            shown(&format!("✕ {} exited", counts.exited)),
            Style::new().fg(Color::Red),
        ),
        gap(),
        (shown(&subagents), Style::new().fg(Color::Magenta)),
        gap(),
        (
            shown(&format!("{machines} · {} online", counts.online)),
            Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
        ),
    ];
    let inset = Rect::new(line.x + 1, line.y, line.width.saturating_sub(1), 1);
    draw::render(draw::paragraph(vec![draw::spans(parts)]), inset, buf);
}

/// The table's columns, each as `(x, width)` within its lines, a blank column after it included. `None` is left out.
struct Columns {
    glyph: u16,
    name: (u16, u16),
    agent: Option<(u16, u16)>,
    machine: Option<(u16, u16)>,
    sub: Option<(u16, u16)>,
    doing: Option<(u16, u16)>,
    pr: Option<(u16, u16)>,
}

const AGENT_WIDTH: u16 = 8;
const MACHINE_WIDTH: u16 = 11;
const SUB_WIDTH: u16 = 5;
const PR_WIDTH: u16 = 10;
const NAME_MIN: u16 = 8;
const NAME_MAX: u16 = 26;
const DOING_MIN: u16 = 12;

impl Columns {
    /// The columns of a table `width` wide: the glyph and the name always, then each other column while it fits.
    fn fit(width: u16) -> Self {
        // The optional columns, in the order they are left out when they don't all fit.
        const OPTIONAL: [u16; 5] = [SUB_WIDTH, AGENT_WIDTH, PR_WIDTH, MACHINE_WIDTH, DOING_MIN];
        let [sub, agent, pr, machine, doing] = [0, 1, 2, 3, 4];
        let mut on = [true; 5];
        let room = width.saturating_sub(3);
        let need = |on: &[bool; 5]| {
            NAME_MIN
                + OPTIONAL
                    .iter()
                    .zip(on)
                    .map(|(w, on)| if *on { *w } else { 0 })
                    .sum::<u16>()
        };
        for i in 0..on.len() {
            if need(&on) <= room {
                break;
            }
            on[i] = false;
        }
        let spare = room.saturating_sub(need(&on));
        let name_extra = if on[doing] {
            (spare / 2).min(NAME_MAX - NAME_MIN)
        } else {
            spare
        };
        let mut x = 3;
        let mut place = |on: bool, width: u16| {
            on.then(|| {
                let at = (x, width);
                x += width;
                at
            })
        };
        let name = place(true, NAME_MIN + name_extra).unwrap_or((3, 0));
        let agent = place(on[agent], AGENT_WIDTH);
        let machine = place(on[machine], MACHINE_WIDTH);
        let sub = place(on[sub], SUB_WIDTH);
        let doing = place(on[doing], DOING_MIN + spare - name_extra);
        let pr = place(on[pr], PR_WIDTH);
        Self {
            glyph: 1,
            name,
            agent,
            machine,
            sub,
            doing,
            pr,
        }
    }
}

/// Draws `text` in column `col` of line `at`, cut to the column less its blank.
fn cell(buf: &mut Buffer, at: Rect, col: (u16, u16), text: &str, style: Style) {
    let (x, width) = col;
    let room = usize::from(width.saturating_sub(1));
    if room == 0 || x >= at.width {
        return;
    }
    let room = room.min(usize::from(at.width - x));
    draw::set_cut(buf, at.x + x, at.y, &shown(text), room, style);
}

fn render_names(cols: &Columns, line: Rect, buf: &mut Buffer) {
    let style = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    let named = [
        (Some(cols.name), "NAME"),
        (cols.agent, "AGENT"),
        (cols.machine, "MACHINE"),
        (cols.sub, "SUB"),
        (cols.doing, "DOING"),
        (cols.pr, "PR"),
    ];
    for (col, name) in named {
        if let Some(col) = col {
            cell(buf, line, col, name, style);
        }
    }
}

fn group_label(group: Group) -> (&'static str, Style) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match group {
        Group::NeedsYou => ("NEEDS YOU", bold.fg(Color::Yellow)),
        Group::Working => ("WORKING", bold.fg(Color::Green)),
        Group::Idle => ("IDLE", bold),
        Group::Exited => ("EXITED", bold.fg(Color::Red)),
    }
}

fn render_group(group: Group, count: usize, line: Rect, buf: &mut Buffer) {
    let (label, style) = group_label(group);
    let parts = vec![
        (shown(label), style),
        (
            shown(&format!("  {count}")),
            Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
        ),
    ];
    let inset = Rect::new(line.x + 1, line.y, line.width.saturating_sub(1), 1);
    draw::render(draw::paragraph(vec![draw::spans(parts)]), inset, buf);
}

fn render_machine(
    client: &Client,
    group: &MachineGroup,
    count: usize,
    line: Rect,
    buf: &mut Buffer,
) {
    let offline = !group.local && group.status != MachineStatus::Connected;
    let mut name = machine::style(client, &group.id).add_modifier(Modifier::BOLD);
    let mut meta = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    if offline {
        name = name.add_modifier(Modifier::DIM);
        meta = meta.fg(Color::Red);
    }
    let parts = vec![
        (shown(&group.name), name),
        (shown(&format!("  {}", machine_status(group))), meta),
        (
            shown(&format!("  · {count}")),
            Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
        ),
    ];
    let inset = Rect::new(line.x + 1, line.y, line.width.saturating_sub(1), 1);
    draw::render(draw::paragraph(vec![draw::spans(parts)]), inset, buf);
}

fn agent_name(agent: Option<AgentKind>) -> (&'static str, Style) {
    match agent {
        Some(AgentKind::ClaudeCode) => ("claude", Style::new().fg(Color::LightRed)),
        Some(AgentKind::Codex) => ("codex", Style::new().fg(Color::Gray)),
        Some(AgentKind::Shell) => ("shell", Style::new().fg(Color::Gray)),
        None => (NONE, Style::new().fg(Color::DarkGray)),
    }
}

/// What a session is doing, as its row says it, and how.
fn doing(client: &Client, session: &ClientSession) -> (String, Style) {
    let gray = Style::new().fg(Color::Gray);
    if client.offline(&session.id) {
        let name = client.machine_label(&session.machine);
        return (format!("{name} offline"), Style::new().fg(Color::Red));
    }
    if let Some(why) = &session.failed {
        return (why.clone(), Style::new().fg(Color::Red));
    }
    if session.removing_worktree {
        return ("removing worktree…".to_owned(), gray);
    }
    if session.creating_worktree {
        return ("creating worktree…".to_owned(), gray);
    }
    let state = client.state(&session.id);
    let yellow = Style::new().fg(Color::Yellow);
    if !session.exited
        && let Some(asked) = state.and_then(|s| s.asked())
    {
        return (format!("asks: {asked}"), yellow);
    }
    match &session.activity {
        _ if session.exited => ("exited".to_owned(), gray),
        Activity::Exited { code: Some(code) } => (format!("exited ({code})"), gray),
        Activity::Exited { code: None } => ("exited".to_owned(), gray),
        Activity::NeedsInput => ("asks a question".to_owned(), yellow),
        Activity::NeedsPermission { title } => (format!("asks: {title}"), yellow),
        Activity::Working { tool: Some(tool) } => (tool.title.clone(), gray),
        Activity::Working { tool: None } => ("working".to_owned(), gray),
        Activity::Idle => ("at its prompt".to_owned(), gray),
        Activity::Starting if state.is_some_and(|s| s.at_prompt()) => {
            ("at its prompt".to_owned(), gray)
        }
        Activity::Starting => ("starting".to_owned(), gray),
        Activity::Unknown => (NONE.to_owned(), gray),
    }
}

fn pr_style(session: &ClientSession) -> Style {
    match &session.pr {
        None => Style::new().fg(Color::DarkGray),
        Some(pr) => match (&pr.state, &pr.checks) {
            (PrState::Open, PrChecks::Passing) => Style::new().fg(Color::Green),
            (PrState::Open, PrChecks::Failing) => Style::new().fg(Color::Red),
            (PrState::Open, PrChecks::Pending) => Style::new().fg(Color::Yellow),
            _ => Style::new().fg(Color::Gray),
        },
    }
}

fn render_row(
    client: &Client,
    session: &ClientSession,
    is_selected: bool,
    cols: &Columns,
    line: Rect,
    buf: &mut Buffer,
) {
    if is_selected {
        buf.set_style(line, Style::new().bg(SELECTED_BG));
    }
    let (symbol, color) = glyph(session);
    cell(buf, line, (cols.glyph, 2), symbol, Style::new().fg(color));
    let mut name = Style::new();
    if is_selected {
        name = name.add_modifier(Modifier::BOLD);
    }
    cell(buf, line, cols.name, &session.name, name);
    if let Some(col) = cols.agent {
        let (agent, style) = agent_name(session.agent);
        cell(buf, line, col, agent, style);
    }
    if let Some(col) = cols.machine {
        let label = cut(
            client.machine_label(&session.machine),
            usize::from(MACHINE_WIDTH),
        );
        cell(
            buf,
            line,
            col,
            &label,
            machine::style(client, &session.machine),
        );
    }
    if let Some(col) = cols.sub {
        let count = session.subagents.len();
        let blocked = session
            .subagents
            .iter()
            .any(|s| matches!(s.status(), SubagentStatus::Blocked { .. }));
        let (text, style) = match (count, blocked) {
            (0, _) => (NONE.to_owned(), Style::new().fg(Color::DarkGray)),
            (n, true) => (format!("↳{n}"), Style::new().fg(Color::Yellow)),
            (n, false) => (format!("↳{n}"), Style::new().fg(Color::Magenta)),
        };
        cell(buf, line, col, &text, style);
    }
    if let Some(col) = cols.doing {
        let (text, style) = doing(client, session);
        cell(buf, line, col, &text, style);
    }
    if let Some(col) = cols.pr {
        cell(buf, line, col, &pr_label(session), pr_style(session));
    }
    if client.offline(&session.id) {
        buf.set_style(line, Style::new().add_modifier(Modifier::DIM));
    }
}
