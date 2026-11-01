//! The status page's table (T57): every listed session on every machine in one table, grouped by what the user owes it
//! ([`Group`], from [`SessionStatus::in_client`]) or by machine ([`Grouping`]), with a count of each group.
//!
//! The table's lines ([`Client::status_lines`]) are the TUI's to draw. Which session the table has selected, the
//! grouping and the scroll are this window's own: selecting a row of the table doesn't change the sidebar's selection.
//! Attaching ([`Intent::StatusAttach`](super::Intent::StatusAttach)) does: it selects the session in this window and
//! focuses a pane showing it (see [`Client::status_attach`]).
//!
//! The TUI says how many lines the table shows at once (`shown`) with each intent that moves the selection, so the
//! selection is scrolled into view; the wheel scrolls the table and leaves the selection where it is.

use super::automation::SessionStatus;
use super::{Client, ClientCommand, ClientSession, MachineGroup, MachineStatus, scrolled};
use crate::ids::SessionId;
use crate::layout::{Content, Layout, LayoutOp, PageId, Pane, PaneId, ViewKind};
use crate::wire::{ClientRequest, UiApply};

/// What the status page's table is grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// What the user owes each session: needs you, working, idle, exited.
    #[default]
    State,
    /// The machine each session is on, in the sidebar's order.
    Machine,
}

impl Grouping {
    /// The other grouping: what the toggle switches to.
    pub fn other(self) -> Self {
        match self {
            Self::State => Self::Machine,
            Self::Machine => Self::State,
        }
    }
}

/// What the user owes a session, in the order the table lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// Its agent (or a subagent of its) waits on the user.
    NeedsYou,
    /// Its agent is in a turn, still starting, or it runs no agent.
    Working,
    /// Its agent is at its prompt.
    Idle,
    /// Its process ended.
    Exited,
}

impl Group {
    pub const ALL: [Group; 4] = [Group::NeedsYou, Group::Working, Group::Idle, Group::Exited];

    /// `session`'s group, from its status as scripts see it.
    pub fn of(client: &Client, session: &ClientSession) -> Self {
        match SessionStatus::in_client(client, session) {
            SessionStatus::WantsUser => Self::NeedsYou,
            SessionStatus::Running => Self::Working,
            SessionStatus::Idle => Self::Idle,
            SessionStatus::Exited => Self::Exited,
        }
    }
}

/// One line of the table, top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLine<'a> {
    /// A state's header (grouped by state), with how many sessions it has. Only groups with sessions have one.
    Group {
        group: Group,
        count: usize,
    },
    /// A machine's header (grouped by machine), with how many sessions it has. Every machine has one.
    Machine {
        machine: MachineGroup,
        count: usize,
    },
    Session(&'a ClientSession),
    /// A blank line between two groups.
    Gap,
}

/// What the header line counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusCounts {
    pub needs_you: usize,
    pub working: usize,
    pub idle: usize,
    pub exited: usize,
    /// Subagents listed across every session.
    pub subagents: usize,
    /// Machines: this one and the trusted peers.
    pub machines: usize,
    /// Machines connected now.
    pub online: usize,
}

impl StatusCounts {
    pub fn of(&self, group: Group) -> usize {
        match group {
            Group::NeedsYou => self.needs_you,
            Group::Working => self.working,
            Group::Idle => self.idle,
            Group::Exited => self.exited,
        }
    }
}

/// The table's view state, this window's own.
#[derive(Debug, Clone, Default)]
pub(super) struct StatusTable {
    grouping: Grouping,
    /// The session the table has selected, once the user picked one.
    selected: Option<SessionId>,
    /// The index of the first line shown, before clamping to the lines.
    scroll: usize,
}

impl Client {
    /// What the status page's table is grouped by.
    pub fn status_grouping(&self) -> Grouping {
        self.status.grouping
    }

    /// The status page's table, top to bottom. By state: each group with sessions, in [`Group`] order, its header
    /// then its sessions in the sidebar's order, a gap between groups. By machine: each machine in the sidebar's
    /// order, its header then its sessions by group (in the sidebar's order within one), no gaps.
    pub fn status_lines(&self) -> Vec<StatusLine<'_>> {
        let grouped: Vec<(&ClientSession, Group)> = self
            .sessions
            .iter()
            .map(|s| (s, Group::of(self, s)))
            .collect();
        let mut lines = Vec::with_capacity(grouped.len() + 8);
        match self.status.grouping {
            Grouping::State => {
                for group in Group::ALL {
                    let members: Vec<&ClientSession> = grouped
                        .iter()
                        .filter(|(_, g)| *g == group)
                        .map(|(s, _)| *s)
                        .collect();
                    if members.is_empty() {
                        continue;
                    }
                    if !lines.is_empty() {
                        lines.push(StatusLine::Gap);
                    }
                    lines.push(StatusLine::Group {
                        group,
                        count: members.len(),
                    });
                    lines.extend(members.into_iter().map(StatusLine::Session));
                }
            }
            Grouping::Machine => {
                for machine in self.machines() {
                    let mut members: Vec<(&ClientSession, Group)> = grouped
                        .iter()
                        .filter(|(s, _)| s.machine == machine.id)
                        .copied()
                        .collect();
                    // Stable: the sidebar's order within a group.
                    members.sort_by_key(|(_, g)| *g);
                    lines.push(StatusLine::Machine {
                        count: members.len(),
                        machine,
                    });
                    lines.extend(members.into_iter().map(|(s, _)| StatusLine::Session(s)));
                }
                // A session on a machine the client no longer lists (none should be) is still shown.
                let machines = self.machines();
                lines.extend(
                    grouped
                        .iter()
                        .filter(|(s, _)| !machines.iter().any(|m| m.id == s.machine))
                        .map(|(s, _)| StatusLine::Session(s)),
                );
            }
        }
        lines
    }

    /// What the status page's header line counts.
    pub fn status_counts(&self) -> StatusCounts {
        let mut counts = StatusCounts::default();
        for session in &self.sessions {
            match Group::of(self, session) {
                Group::NeedsYou => counts.needs_you += 1,
                Group::Working => counts.working += 1,
                Group::Idle => counts.idle += 1,
                Group::Exited => counts.exited += 1,
            }
            counts.subagents += session.subagents.len();
        }
        let machines = self.machines();
        counts.machines = machines.len();
        counts.online = machines
            .iter()
            .filter(|m| m.status == MachineStatus::Connected)
            .count();
        counts
    }

    /// The session the status page's table has selected: the one the user picked while it is listed, else the table's
    /// first. `None` with no sessions.
    pub fn status_selected(&self) -> Option<&SessionId> {
        let lines = self.status_lines();
        let first = lines.iter().find_map(|l| match l {
            StatusLine::Session(s) => Some(&s.id),
            _ => None,
        });
        match &self.status.selected {
            Some(id) if self.listed(id) => Some(id),
            _ => first,
        }
    }

    /// The index of the first line the status page's table shows, before the TUI clamps it to the lines.
    pub fn status_scroll(&self) -> usize {
        self.status.scroll
    }

    /// Where the table's selected session is among its lines.
    fn status_selected_line(&self) -> Option<usize> {
        let selected = self.status_selected()?;
        self.status_lines()
            .iter()
            .position(|l| matches!(l, StatusLine::Session(s) if &s.id == selected))
    }

    /// Scrolls the least that shows the table's selected session, when it shows `shown` lines at once.
    fn status_scroll_to_selected(&mut self, shown: usize) {
        let max = self.status_lines().len().saturating_sub(shown);
        let first = self.status.scroll.min(max);
        let Some(index) = self.status_selected_line() else {
            self.status.scroll = first;
            return;
        };
        if shown == 0 {
            return;
        }
        self.status.scroll = if index < first {
            index
        } else if index >= first + shown {
            index + 1 - shown
        } else {
            first
        };
    }

    pub(super) fn status_group_by(&mut self, by: Grouping, shown: usize) -> Vec<ClientCommand> {
        self.status.grouping = by;
        self.status_scroll_to_selected(shown);
        Vec::new()
    }

    pub(super) fn status_select(&mut self, session: SessionId, shown: usize) -> Vec<ClientCommand> {
        if self.listed(&session) {
            self.status.selected = Some(session);
            self.status_scroll_to_selected(shown);
        }
        Vec::new()
    }

    /// Moves the table's selection `by` sessions (down for positive), stopping at either end.
    pub(super) fn status_step(&mut self, by: i32, shown: usize) -> Vec<ClientCommand> {
        let order: Vec<SessionId> = self
            .status_lines()
            .iter()
            .filter_map(|l| match l {
                StatusLine::Session(s) => Some(s.id.clone()),
                _ => None,
            })
            .collect();
        if order.is_empty() {
            return Vec::new();
        }
        let at = self
            .status_selected()
            .and_then(|id| order.iter().position(|o| o == id))
            .unwrap_or(0);
        let to = scrolled(at, by, order.len() - 1);
        self.status.selected = Some(order[to].clone());
        self.status_scroll_to_selected(shown);
        Vec::new()
    }

    pub(super) fn status_scroll_by(&mut self, by: i32, shown: usize) -> Vec<ClientCommand> {
        let max = self.status_lines().len().saturating_sub(shown);
        self.status.scroll = scrolled(self.status.scroll.min(max), by, max);
        Vec::new()
    }

    /// Attaches session `session` (`⏎` or a second click on its row): the table and this window select it, and the
    /// window focuses a pane showing it. That is the first pane, the current page's first, then each other page's in
    /// order, that shows the selection or has the session pinned; one on another page is shown by asking the host to
    /// select its page. With none, the host is asked for a new page showing the selection.
    pub(super) fn status_attach(&mut self, session: SessionId) -> Vec<ClientCommand> {
        if !self.listed(&session) {
            return Vec::new();
        }
        self.status.selected = Some(session.clone());
        let home = self.home(&session);
        let layout = self.host_layout();
        let shows = |content: &Content| match content {
            Content::Selected => true,
            Content::Session { machine, id } => {
                id == &session && self.content_home(machine.as_ref()) == home
            }
            _ => false,
        };
        let found = find_leaf(&layout, shows);
        self.focus = super::Focus::Terminal;
        self.chosen = true;
        let mut commands = Vec::new();
        match found {
            Some((page, pane)) => {
                self.focused = Some(pane);
                let selected = self.select(Some(session));
                let resized = !selected.is_empty();
                commands.extend(selected);
                if page != layout.current() {
                    commands.extend(self.user_request(ClientRequest::UiApply(UiApply {
                        op: LayoutOp::PageSelect { page },
                    })));
                } else if !resized {
                    commands.extend(self.resize_focused());
                }
            }
            None => {
                commands.extend(self.select(Some(session)));
                commands.extend(self.user_request(ClientRequest::UiApply(UiApply {
                    op: LayoutOp::PageNew {
                        name: None,
                        content: Content::Selected,
                    },
                })));
            }
        }
        commands
    }

    /// Shows the status page (`ctrl-b s`): focuses the first pane showing it, asking the host to select its page if it
    /// isn't the current one, or asks the host for a new page showing it.
    pub(super) fn open_status(&mut self) -> Vec<ClientCommand> {
        let layout = self.host_layout();
        let found = find_leaf(&layout, |c| *c == Content::View(ViewKind::Status));
        self.focus = super::Focus::Terminal;
        match found {
            Some((page, pane)) => {
                self.focused = Some(pane);
                if page == layout.current() {
                    return Vec::new();
                }
                self.user_request(ClientRequest::UiApply(UiApply {
                    op: LayoutOp::PageSelect { page },
                }))
            }
            None => self.user_request(ClientRequest::UiApply(UiApply {
                op: LayoutOp::PageNew {
                    name: Some(STATUS_PAGE.to_owned()),
                    content: Content::View(ViewKind::Status),
                },
            })),
        }
    }
}

/// What a page opened for the status page is called.
pub const STATUS_PAGE: &str = "status";

/// The first leaf whose content `wanted` takes, the current page's leaves first, then each other page's in order: its
/// page and its id.
fn find_leaf(layout: &Layout, wanted: impl Fn(&Content) -> bool) -> Option<(PageId, PaneId)> {
    fn walk(pane: &Pane, wanted: &dyn Fn(&Content) -> bool) -> Option<PaneId> {
        match pane {
            Pane::Leaf { id, content } => wanted(content).then_some(*id),
            Pane::Split { a, b, .. } => walk(a, wanted).or_else(|| walk(b, wanted)),
        }
    }
    let current = layout.current_page();
    let others = layout.pages().iter().filter(|p| p.id != current.id);
    std::iter::once(current)
        .chain(others)
        .find_map(|page| walk(&page.root, &wanted).map(|pane| (page.id, pane)))
}
