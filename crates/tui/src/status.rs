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
//! - Below the table, a box titled `events` holds the latest events that fit ([`Client::events`]), the newest on its
//!   last line: how long ago, the machine, the session, what happened and what it said (agents' text, made safe and
//!   cut at the box). It is shown only while the table keeps [`TABLE_MIN`] lines.
//! - The page's last line is the message line (T59): `❯`, the target (`[to t28-hello-build]`, `[to all working (3)]`,
//!   …), what is typed (or, with nothing typed, how the last send went, else a hint) and `[send]`. While it takes the
//!   keys a reversed cell marks where typing goes.
//!
//! - The preview (T58) takes the table's right part, while the table keeps [`TABLE_KEEP`] columns and the box is
//!   [`PREVIEW_MIN_HEIGHT`] lines tall: a box titled with the selected session's name (its machine at the right) showing
//!   the bottom of its live screen (the client's emulator for it, read-only: nothing typed or clicked here reaches
//!   it). While its agent asks something ([`Client::status_asked`]), below the screen: what it asks (made safe and cut),
//!   a button for each answer the host would accept (`[y yes] [a always] [n no]`, or `[1 <option>] … [n no]`; one it
//!   would refuse is drawn dim, without brackets, and a note says why), or, for a question it can't answer from here,
//!   a note to answer it in its pane. `[⏎ attach]` always ends the buttons, and the last answer's outcome
//!   ([`Client::answer_report`]) shows on the note line. The preview goes first when the pane is narrow or short.
//!
//! [`hit`] reads the same geometry: a session's row ([`Target::StatusRow`]), the toggle ([`Target::StatusGroupBy`]), a
//! feed line ([`Target::StatusEvent`]), the target ([`Target::StatusTarget`]), `[send]` ([`Target::StatusSend`]),
//! the rest of the message line ([`Target::StatusMessage`]), and the preview's buttons ([`Target::StatusAnswer`],
//! [`Target::StatusPreviewAttach`]); the rest of the preview is the pane ([`Target::Pane`]), never its session.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;
use shepherd_core::acpp::{Choice, PrChecks, PrState, Seq};
use shepherd_core::client::answer::{
    AnswerOutcome, AnswerRefusal, AnswerReport, Answers, Asked, Unanswerable,
};
use shepherd_core::client::events::{EventKind, FeedEvent};
use shepherd_core::client::message::{MessageTarget, Outcome, Refusal, Report, Skip};
use shepherd_core::client::panes::{Area, arrange};
use shepherd_core::client::status::{Group, Grouping, StatusCounts, StatusLine};
use shepherd_core::client::{Client, ClientSession, MachineGroup, MachineStatus};
use shepherd_core::config::AgentKind;
use shepherd_core::layout::{Content, Pane, PaneId, ViewKind};
use shepherd_core::session::{Activity, SubagentStatus};
use shepherd_core::wire::Busy;

use crate::Screens;
use crate::draw::{self, shown};
use crate::layout::Target;
use crate::sidebar::{glyph, machine_status, pr_label};
use crate::width::{columns, cut, cut_left};
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
    /// The events feed's box, below the table, when there is room for it and a table.
    events: Option<Rect>,
    /// The message line, the page's last, when there is room for it and a table.
    message: Option<Message>,
    /// The preview's box, right of the table, when there is room for it.
    preview: Option<Rect>,
}

/// The message line's parts.
struct Message {
    line: Rect,
    /// The target (`[to …]`).
    target: Rect,
    /// What is typed, or the hint or last report while nothing is.
    field: Rect,
    /// `[send]`, when it fits.
    send: Option<Rect>,
}

/// The events feed's box is at most this tall, borders included.
const EVENTS_MAX: u16 = 9;
/// The table's box keeps at least this many lines (six of its own) when the events feed is shown.
const TABLE_MIN: u16 = 9;
const SEND: &str = "[send]";
/// What a line break in the message line's text is drawn as.
const NEWLINE: &str = "↵";

struct Table {
    frame: Rect,
    /// The line of column names.
    names: Rect,
    /// The table's lines.
    lines: Rect,
}

/// The page in `area`, top to bottom: the line of counts, the table, the events feed (a third of what the table and
/// it share, at most [`EVENTS_MAX`], and only while the table keeps [`TABLE_MIN`]) and the message line. A window too
/// small for a table has only the counts.
fn geometry(client: &Client, area: Rect) -> Geometry {
    let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let mut geometry = Geometry {
        header,
        table: None,
        events: None,
        message: None,
        preview: None,
    };
    if area.height < 4 || area.width < 12 {
        return geometry;
    }
    let mut rest = area.height - 1;
    if area.height >= 9 && area.width >= 24 {
        rest -= 1;
        geometry.message = Some(message_parts(
            client,
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        ));
    }
    let events = (rest / 3).min(EVENTS_MAX);
    let events = if events >= 3 && rest - events >= TABLE_MIN {
        events
    } else {
        0
    };
    let frame = Rect::new(area.x, area.y + 1, area.width, rest - events);
    if events > 0 {
        geometry.events = Some(Rect::new(area.x, frame.bottom(), area.width, events));
    }
    let (frame, preview) = split_preview(client, frame);
    geometry.preview = preview;
    let inner = Rect::new(frame.x + 1, frame.y + 1, frame.width - 2, frame.height - 2);
    geometry.table = Some(Table {
        frame,
        names: Rect::new(inner.x, inner.y, inner.width, 1),
        lines: Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
    });
    geometry
}

/// Where the message line's parts are on `line`: `❯ `, the target, a blank, the field, a blank and `[send]` at the
/// right, if it fits after a field of a few columns.
fn message_parts(client: &Client, line: Rect) -> Message {
    let start = line.x + 3;
    let label = target_label(client);
    let room = line.right().saturating_sub(start);
    let width = u16::try_from(columns(&label)).unwrap_or(u16::MAX).min(room);
    let target = Rect::new(start, line.y, width, 1);
    let send_width = u16::try_from(columns(SEND)).unwrap_or(0);
    let send_x = line.right().saturating_sub(send_width + 1);
    let field_x = target.right() + 1;
    let send = (send_x >= field_x + 6).then(|| Rect::new(send_x, line.y, send_width, 1));
    let field_end = send.map_or(line.right(), |s| s.x.saturating_sub(1));
    let field = Rect::new(field_x, line.y, field_end.saturating_sub(field_x), 1);
    Message {
        line,
        target,
        field,
        send,
    }
}

/// How many of the table's lines a status page in `area` shows at once.
fn shown_in(client: &Client, area: Rect) -> usize {
    geometry(client, area)
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
    let geometry = geometry(client, area);
    let table = geometry.table?;
    let shown = usize::from(table.lines.height);
    if let Some(frame) = geometry.preview.filter(|p| p.contains(at)) {
        let found = preview(client, frame).and_then(|p| {
            p.buttons
                .into_iter()
                .find(|b| b.area.contains(at))
                .and_then(|b| match b.target? {
                    Press::Answer { asked, choice } => Some(Target::StatusAnswer {
                        pane,
                        session: p.session.id.clone(),
                        asked,
                        choice,
                    }),
                    Press::Attach => Some(Target::StatusPreviewAttach {
                        pane,
                        session: p.session.id.clone(),
                    }),
                })
        });
        return Some(found.unwrap_or(Target::Pane(pane)));
    }
    if let Some(message) = geometry.message.filter(|m| m.line.contains(at)) {
        if message.target.contains(at) {
            let next = client.next_message_target();
            return Some(Target::StatusTarget { pane, next });
        }
        if message.send.is_some_and(|s| s.contains(at)) {
            return Some(Target::StatusSend { pane });
        }
        return Some(Target::StatusMessage { pane });
    }
    if let Some(events) = geometry.events.filter(|e| e.contains(at)) {
        return feed_lines(client, events)
            .into_iter()
            .find(|(_, line)| line.contains(at))
            .map(|(event, _)| Target::StatusEvent {
                pane,
                session: event.session.clone(),
                shown,
            });
    }
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
    focused_area(client).map_or(0, |area| shown_in(client, area))
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
        .map(|area| shown_in(client, area))
}

// ---------- drawing ----------

/// Draws the status page in `area`, the preview from `screens`.
pub(crate) fn render(client: &Client, screens: &Screens, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let geometry = geometry(client, area);
    render_counts(&client.status_counts(), geometry.header, buf);
    let Some(table) = geometry.table else {
        return;
    };
    if let Some(events) = geometry.events {
        render_events(client, events, buf);
    }
    if let Some(message) = &geometry.message {
        render_message(client, message, buf);
    }
    if let Some(frame) = geometry.preview {
        render_preview(client, screens, frame, buf);
    }
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

// ---------- the events feed ----------

/// The feed's latest events that fit inside its box `frame`, each with its line: the newest on the last line.
fn feed_lines(client: &Client, frame: Rect) -> Vec<(&FeedEvent, Rect)> {
    let inner = Rect::new(
        frame.x + 1,
        frame.y + 1,
        frame.width.saturating_sub(2),
        frame.height.saturating_sub(2),
    );
    let shown = usize::from(inner.height);
    let count = client.events().len();
    let rows = inner.rows().skip(shown.saturating_sub(count));
    client
        .events()
        .skip(count.saturating_sub(shown))
        .zip(rows)
        .collect()
}

const AGE_WIDTH: u16 = 6;
const EVENT_MACHINE_WIDTH: u16 = 12;
const EVENT_NAME_MIN: u16 = 8;
const EVENT_NAME_MAX: u16 = 22;
const EVENT_KIND_WIDTH: u16 = 11;
/// The feed's lines are this wide at least to have a machine column.
const EVENT_MACHINE_FROM: u16 = 70;

fn render_events(client: &Client, frame: Rect, buf: &mut Buffer) {
    let border = Style::new().fg(Color::DarkGray);
    draw::render(Block::bordered().border_style(border), frame, buf);
    let title_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    draw::set(
        buf,
        frame.x + 1,
        frame.y,
        &shown(" events "),
        usize::from(frame.width.saturating_sub(3)),
        title_style,
    );
    let gray = Style::new().fg(Color::Gray);
    let dim = gray.add_modifier(Modifier::DIM);
    for (event, line) in feed_lines(client, frame) {
        let (glyph, label, color) = event_label(&event.kind);
        // The session's current name while it is listed, else the one it had.
        let name = client
            .sessions()
            .iter()
            .find(|s| s.id == event.session)
            .map_or(event.name.as_str(), |s| s.name.as_str());
        let age = event
            .age(client)
            .map(|age| age_label(age.as_secs()))
            .unwrap_or_default();
        let machine_width = match line.width >= EVENT_MACHINE_FROM {
            true => EVENT_MACHINE_WIDTH,
            false => 0,
        };
        let machine_label = cut(
            client.machine_label(&event.machine),
            usize::from(EVENT_MACHINE_WIDTH - 1),
        );
        let name_width = (line.width / 4).clamp(EVENT_NAME_MIN, EVENT_NAME_MAX);
        let columns = [
            (AGE_WIDTH, format!("{age:>4}"), dim),
            (
                machine_width,
                machine_label,
                machine::style(client, &event.machine),
            ),
            (name_width, name.to_owned(), Style::new()),
            (2, glyph.to_owned(), Style::new().fg(color)),
            (EVENT_KIND_WIDTH, label.to_owned(), Style::new().fg(color)),
        ];
        let mut x = 1;
        for (width, text, style) in columns {
            cell(buf, line, (x, width), &text, style);
            x += width;
        }
        let rest = line.width.saturating_sub(x) + 1;
        cell(buf, line, (x, rest), &event_detail(&event.kind), gray);
    }
}

/// How old an event `secs` seconds old is, in its largest whole unit: `12s`, `3m`, `2h` or `4d`.
fn age_label(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// An event's glyph, what it is called, and its color.
fn event_label(kind: &EventKind) -> (&'static str, &'static str, Color) {
    match kind {
        EventKind::Created => ("+", "created", Color::Gray),
        EventKind::Started { .. } => ("●", "started", Color::Green),
        EventKind::Asked { .. } => ("▲", "asked", Color::Yellow),
        EventKind::Answered => ("●", "answered", Color::Green),
        EventKind::Prompted { .. } => ("❯", "prompted", Color::Cyan),
        EventKind::Finished => ("○", "finished", Color::Gray),
        EventKind::Exited { .. } => ("✕", "exited", Color::Red),
        EventKind::Pr { pr } => match pr.as_ref().map(|p| (&p.state, &p.checks)) {
            Some((PrState::Open, PrChecks::Passing)) => ("✓", "PR", Color::Green),
            Some((PrState::Open, PrChecks::Failing)) => ("✗", "PR", Color::Red),
            Some((PrState::Open, PrChecks::Pending)) => ("…", "PR", Color::Yellow),
            _ => ("○", "PR", Color::Gray),
        },
        EventKind::Closed => ("✕", "closed", Color::Gray),
    }
}

/// What an event says beyond its kind: agents' and other clients' text, made safe when drawn.
fn event_detail(kind: &EventKind) -> String {
    match kind {
        EventKind::Created | EventKind::Answered | EventKind::Finished | EventKind::Closed => {
            String::new()
        }
        EventKind::Started { agent } => agent.clone(),
        EventKind::Asked { text: Some(text) } => text.clone(),
        EventKind::Asked { text: None } => "a question".to_owned(),
        // `by` is the asking client's claim: a label, nothing more.
        EventKind::Prompted { text, by } => format!("by {by}: {text}"),
        EventKind::Exited { code: Some(code) } => format!("code {code}"),
        EventKind::Exited { code: None } => String::new(),
        EventKind::Pr { pr: None } => "no PR".to_owned(),
        EventKind::Pr { pr: Some(pr) } => {
            let state = match &pr.state {
                PrState::Open => "open",
                PrState::Closed => "closed",
                PrState::Merged => "merged",
                PrState::Other(other) => other,
            };
            let checks = match &pr.checks {
                PrChecks::None => "no checks",
                PrChecks::Pending => "checks pending",
                PrChecks::Passing => "checks passing",
                PrChecks::Failing => "checks failing",
                PrChecks::Other(other) => other,
            };
            format!("#{} {state} · {checks}", pr.number)
        }
    }
}

// ---------- the message line ----------

/// What the target says: `[to <name>]` for the selected session, else the group and how many it has now.
fn target_label(client: &Client) -> String {
    let count = client.message_recipients().len();
    let to = match client.message_target() {
        MessageTarget::Selected => {
            let name = client
                .status_selected()
                .and_then(|id| client.sessions().iter().find(|s| &s.id == id))
                .map_or("nobody".to_owned(), |s| cut(&s.name, 24));
            format!("to {name}")
        }
        MessageTarget::Working => format!("to all working ({count})"),
        MessageTarget::Idle => format!("to all idle ({count})"),
        MessageTarget::Machine(machine) => {
            let name = cut(client.machine_label(machine), 16);
            format!("to all on {name} ({count})")
        }
    };
    format!("[{to}]")
}

fn render_message(client: &Client, message: &Message, buf: &mut Buffer) {
    let line = message.line;
    let prompt = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    draw::set(buf, line.x + 1, line.y, &shown("❯"), 1, prompt);
    let target = message.target;
    draw::set_cut(
        buf,
        target.x,
        target.y,
        &shown(&target_label(client)),
        usize::from(target.width),
        Style::new().fg(Color::Blue),
    );
    if let Some(send) = message.send {
        let style = Style::new().fg(Color::Cyan);
        draw::set(
            buf,
            send.x,
            send.y,
            &shown(SEND),
            usize::from(send.width),
            style,
        );
    }
    let field = message.field;
    let room = usize::from(field.width);
    if room == 0 {
        return;
    }
    let cursor = Style::new().add_modifier(Modifier::REVERSED);
    let text = client.message_text();
    if !text.is_empty() {
        // One line: each line break shows as a glyph (`display_safe` would drop it).
        let text = text.replace('\n', NEWLINE);
        let typed = cut_left(&text, room.saturating_sub(usize::from(client.composing())));
        let (end, _) = draw::set(buf, field.x, field.y, &shown(&typed), room, Style::new());
        if client.composing() && end < field.right() {
            buf.set_style(Rect::new(end, field.y, 1, 1), cursor);
        }
        return;
    }
    let mut x = field.x;
    if client.composing() {
        buf.set_style(Rect::new(x, field.y, 1, 1), cursor);
        x += 2;
    }
    let (hint, style) = match client.message_report() {
        Some(report) => report_line(report),
        None => (
            "click to message · tab: widen the target".to_owned(),
            Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
        ),
    };
    let room = usize::from(field.right().saturating_sub(x));
    draw::set_cut(buf, x, field.y, &shown(&hint), room, style);
}

/// How the last send went, in a line: who refused it and why, how many it is being and was sent to, then who was
/// skipped and why.
fn report_line(report: &Report) -> (String, Style) {
    if report.deliveries.is_empty() {
        return (
            "no one to message".to_owned(),
            Style::new().fg(Color::Yellow),
        );
    }
    let mut sent = 0;
    let mut sending = 0;
    let mut refused = Vec::new();
    let mut skipped = Vec::new();
    for delivery in &report.deliveries {
        match &delivery.outcome {
            Outcome::Sending => sending += 1,
            Outcome::Sent => sent += 1,
            Outcome::Refused(why) => {
                let why = match why {
                    Refusal::Busy(Busy::Running) => "busy",
                    Refusal::Busy(Busy::WantsUser) => "wants-user",
                    Refusal::Other(why) => why,
                };
                refused.push(format!("{} ({why})", delivery.name));
            }
            Outcome::Skipped(why) => {
                let why = match why {
                    Skip::Exited => "exited",
                    Skip::NoAgent => "no agent",
                    Skip::Offline => "offline",
                };
                skipped.push(format!("{} ({why})", delivery.name));
            }
        }
    }
    // Who refused first: it is what the user has to act on, and the line is cut at its end.
    let mut parts = Vec::new();
    if !refused.is_empty() {
        parts.push(format!("refused: {}", refused.join(", ")));
    }
    if sending > 0 {
        parts.push(format!("sending to {sending}…"));
    }
    if sent > 0 || (sending == 0 && refused.is_empty()) {
        parts.push(format!("sent to {sent}"));
    }
    if !skipped.is_empty() {
        parts.push(format!("skipped: {}", skipped.join(", ")));
    }
    let style = if refused.is_empty() {
        Style::new().fg(Color::Gray)
    } else {
        Style::new().fg(Color::Yellow)
    };
    (parts.join(" · "), style)
}

// ---------- the preview ----------

/// The table keeps at least this many columns, borders included, beside the preview.
const TABLE_KEEP: u16 = 64;
/// The preview's narrowest width, borders included.
const PREVIEW_MIN: u16 = 36;
/// The preview is shown only in a box at least this tall, borders included.
const PREVIEW_MIN_HEIGHT: u16 = 8;
/// The most lines the preview's buttons take.
const BUTTON_LINES: usize = 2;
/// The most columns of an option's text on its button.
const OPTION_MAX: usize = 20;
const ATTACH: &str = "[⏎ attach]";

/// The table's box `frame` less the preview's part, and the preview's box: two fifths of it, but leaving the table
/// [`TABLE_KEEP`] columns. No preview with nothing selected, or when it doesn't fit.
fn split_preview(client: &Client, frame: Rect) -> (Rect, Option<Rect>) {
    let fits = frame.width >= TABLE_KEEP + PREVIEW_MIN && frame.height >= PREVIEW_MIN_HEIGHT;
    if !fits || client.status_selected().is_none() {
        return (frame, None);
    }
    let width = (frame.width / 5 * 2)
        .min(frame.width - TABLE_KEEP)
        .max(PREVIEW_MIN);
    let table = Rect {
        width: frame.width - width,
        ..frame
    };
    let preview = Rect::new(table.right(), frame.y, width, frame.height);
    (table, Some(preview))
}

/// What a preview button does when clicked.
#[derive(Debug, Clone, Copy)]
enum Press {
    Answer { asked: Seq, choice: Choice },
    Attach,
}

struct Button {
    area: Rect,
    label: String,
    style: Style,
    /// `None` for an answer the host would refuse: drawn, not clickable.
    target: Option<Press>,
}

/// The preview's parts in its box.
struct Preview<'a> {
    session: &'a ClientSession,
    /// Where the session's screen is drawn.
    screen: Rect,
    /// The line between the screen and what is below it.
    rule: Rect,
    /// What the agent asks (raw), and its line.
    question: Option<(Rect, String)>,
    buttons: Vec<Button>,
    /// A note: the last answer's outcome, why an answer is refused, or why the question can't be answered here.
    note: Option<(Rect, String, Style)>,
}

/// The key that answers `choice`, as its button says.
fn choice_key(choice: Choice) -> String {
    match choice {
        Choice::Yes => "y".to_owned(),
        Choice::Always => "a".to_owned(),
        Choice::No => "n".to_owned(),
        Choice::Option(n) => n.to_string(),
    }
}

/// A label, the style it is drawn in, and what clicking it does.
type Item = (String, Style, Option<Press>);

/// The buttons (and the note) for what `session`'s agent asks, `[⏎ attach]` last; and the seq of the question they
/// answer, if they do.
fn answer_items(
    client: &Client,
    session: &ClientSession,
    asked: Option<&Asked<'_>>,
) -> (Vec<Item>, Option<(String, Style)>, Option<Seq>) {
    let cyan = Style::new().fg(Color::Cyan);
    let dim = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
    let mut items = Vec::new();
    let mut note = None;
    let mut pending = None;
    match asked.map(|a| &a.answers) {
        Some(Answers::Open {
            asked: seq,
            buttons,
        }) => {
            pending = Some(*seq);
            for button in buttons {
                let text = cut(shown(button.label).as_str(), OPTION_MAX);
                let key = choice_key(button.choice);
                match &button.refused {
                    None => {
                        let press = Press::Answer {
                            asked: *seq,
                            choice: button.choice,
                        };
                        items.push((format!("[{key} {text}]"), cyan, Some(press)));
                    }
                    Some(why) => {
                        items.push((format!("{key} {text}"), dim, None));
                        note.get_or_insert((format!("{text}: {why}"), dim));
                    }
                }
            }
        }
        Some(Answers::Unanswerable(why)) => {
            let why = match why {
                Unanswerable::Offline => format!(
                    "{} can't be reached now",
                    client.machine_label(&session.machine)
                ),
                Unanswerable::InPane => "attach to answer it in its pane".to_owned(),
            };
            note = Some((why, Style::new().fg(Color::Yellow)));
        }
        None => {}
    }
    items.push((ATTACH.to_owned(), cyan, Some(Press::Attach)));
    (items, note, pending)
}

/// The preview of the table's selected session in box `frame`.
fn preview(client: &Client, frame: Rect) -> Option<Preview<'_>> {
    let selected = client.status_selected()?;
    let session = client.sessions().iter().find(|s| &s.id == selected)?;
    let inner = Rect::new(
        frame.x + 1,
        frame.y + 1,
        frame.width.saturating_sub(2),
        frame.height.saturating_sub(2),
    );
    let asked = client.asked_of(&session.id);
    let (items, mut note, pending) = answer_items(client, session, asked.as_ref());
    if let Some(report) = client
        .answer_report()
        .filter(|r| r.session == session.id && pending.is_none_or(|seq| seq == r.asked))
    {
        note = Some(report_note(report));
    }
    // The buttons, flowed onto at most [`BUTTON_LINES`] lines; what doesn't fit is left out.
    let mut placed: Vec<(u16, u16, Item)> = Vec::new();
    let (mut line, mut x) = (0u16, 0u16);
    for (label, style, target) in items {
        let room = inner.width.saturating_sub(1);
        let label = cut(&label, usize::from(room));
        let width = u16::try_from(columns(&label)).unwrap_or(u16::MAX);
        if x > 0 && x.saturating_add(width) > room {
            line += 1;
            x = 0;
        }
        if usize::from(line) >= BUTTON_LINES {
            break;
        }
        placed.push((line, x, (label, style, target)));
        x = x.saturating_add(width + 1);
    }
    let lines = placed.last().map_or(0, |(l, ..)| l + 1);
    let below = 1 + u16::from(asked.is_some()) + lines + u16::from(note.is_some());
    let below = below.min(inner.height);
    let screen = Rect::new(inner.x, inner.y, inner.width, inner.height - below);
    let row = |n: u16| Rect::new(inner.x, screen.bottom() + n, inner.width, 1);
    // What is below the rule is inset a column from the border.
    let text_row = |n: u16| {
        let at = row(n);
        Rect::new(at.x + 1, at.y, at.width.saturating_sub(1), 1)
    };
    let mut next = 1;
    let question = asked.as_ref().map(|a| {
        let text = a.text.unwrap_or("it asks something").to_owned();
        next += 1;
        (text_row(next - 1), text)
    });
    let buttons = placed
        .into_iter()
        .map(|(line, x, (label, style, target))| {
            let width = u16::try_from(columns(&label)).unwrap_or(u16::MAX);
            let at = text_row(next + line);
            Button {
                area: Rect::new(at.x + x, at.y, width, 1),
                label,
                style,
                target,
            }
        })
        .collect();
    next += lines;
    let note = note.map(|(text, style)| (text_row(next), text, style));
    Some(Preview {
        session,
        screen,
        rule: row(0),
        question,
        buttons,
        note,
    })
}

/// How the last answer went, for the note line.
fn report_note(report: &AnswerReport) -> (String, Style) {
    let choice = report.choice;
    match &report.outcome {
        AnswerOutcome::Sending => (format!("answering {choice}…"), Style::new().fg(Color::Gray)),
        AnswerOutcome::Answered => (format!("answered {choice}"), Style::new().fg(Color::Green)),
        AnswerOutcome::Refused(why) => {
            let why = match why {
                AnswerRefusal::AlreadyAnswered => "it was already answered".to_owned(),
                AnswerRefusal::NotOffered(why) | AnswerRefusal::Other(why) => why.clone(),
            };
            (
                format!("{choice} refused: {why}"),
                Style::new().fg(Color::Yellow),
            )
        }
    }
}

fn render_preview(client: &Client, screens: &Screens, frame: Rect, buf: &mut Buffer) {
    let Some(preview) = preview(client, frame) else {
        return;
    };
    let session = preview.session;
    let border = Style::new().fg(Color::DarkGray);
    draw::render(Block::bordered().border_style(border), frame, buf);
    let machine_label = cut(client.machine_label(&session.machine), 16);
    let machine_width = u16::try_from(columns(&machine_label)).unwrap_or(0);
    let machine_x = frame.right().saturating_sub(machine_width + 2);
    let title_room = usize::from(machine_x.saturating_sub(frame.x + 2));
    let title = format!(" {} ", shown(&session.name).as_str());
    let title_style = Style::new().add_modifier(Modifier::BOLD);
    draw::set_cut(
        buf,
        frame.x + 1,
        frame.y,
        &shown(&title),
        title_room,
        title_style,
    );
    if machine_x > frame.x + 4 {
        let style = machine::style(client, &session.machine);
        let width = usize::from(machine_width);
        draw::set(
            buf,
            machine_x,
            frame.y,
            &shown(&machine_label),
            width,
            style,
        );
    }
    let area = preview.screen;
    if area.height > 0 {
        match screens.get(&session.id) {
            Some(screen) => {
                let first = (crate::screen::last_used_row(screen) + 1).saturating_sub(area.height);
                crate::screen::render_rows(screen, first, area, buf);
                if client.offline(&session.id) {
                    buf.set_style(area, Style::new().add_modifier(Modifier::DIM));
                }
            }
            None => {
                let gray = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
                let room = usize::from(area.width.saturating_sub(1));
                draw::set_cut(buf, area.x + 1, area.y, &shown("no output yet"), room, gray);
            }
        }
    }
    let rule = preview.rule;
    let line = "─".repeat(usize::from(rule.width));
    draw::set(
        buf,
        rule.x,
        rule.y,
        &shown(&line),
        usize::from(rule.width),
        border,
    );
    if let Some((line, text)) = &preview.question {
        let style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        draw::set(buf, line.x, line.y, &shown("▲"), 1, style);
        let room = usize::from(line.width.saturating_sub(2));
        let text_style = Style::new().fg(Color::Yellow);
        draw::set_cut(buf, line.x + 2, line.y, &shown(text), room, text_style);
    }
    for button in &preview.buttons {
        let area = button.area;
        let width = usize::from(area.width);
        draw::set(
            buf,
            area.x,
            area.y,
            &shown(&button.label),
            width,
            button.style,
        );
    }
    if let Some((line, text, style)) = &preview.note {
        draw::set_cut(
            buf,
            line.x,
            line.y,
            &shown(text),
            usize::from(line.width),
            *style,
        );
    }
}
