//! The host's layout (decision 0022), drawn in the pane area right of the sidebar: the page bar, and the current page's
//! panes, each showing its session, a view or nothing. Where everything goes is core's [`arrange`], which the client
//! also sizes each pane's session by, so what is drawn and what the session is told can't disagree.
//!
//! - The page bar (only with more than one page): each page's name, the current one reversed, then `+` to add one.
//! - A page that is one pane is drawn without a border, exactly as shepherd was before layouts.
//! - Split panes are boxes: the top edge carries the title (the session's name and its machine, in the machine's
//!   color) and, at its right, the pane's menu button `[≡]`. The focused pane's border is bright, the others dim.
//! - A pane shows its session's terminal; a session on a machine that can't be reached now is drawn dimmed (its last
//!   screen, or a placeholder without one). A view (not built yet), an empty pane and a pane too small for a terminal
//!   show a placeholder.
//!
//! [`hit`] says what is under a cell of the pane area: a page, `+`, a divider (dragged to set its split's ratio), a pane's
//! menu button, the focused pane's terminal ([`Target::Terminal`]) or another pane ([`Target::Pane`]).

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Wrap};
use shepherd_core::client::panes::{Area, Arrangement, PanePlace, arrange};
use shepherd_core::client::{Client, MachineStatus};
use shepherd_core::display::DisplayText;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{Content, ViewKind};

use crate::Screens;
use crate::draw::{self, shown};
use crate::layout::Target;
use crate::machine;
use crate::screen::render_screen;
use crate::width::{columns, cut};

/// A pane's menu button, at the right of its top edge.
pub(crate) const MENU_BUTTON: &str = "[≡]";
/// The page bar's button that adds a page.
const NEW_PAGE: &str = "+";
/// The widest a page's name is drawn in the page bar.
const PAGE_NAME_MAX: usize = 16;
/// The widest a machine's or a view's name is drawn in a placeholder.
const NAME_MAX: usize = 24;

/// `rect` as core's [`Area`].
fn area(rect: Rect) -> Area {
    Area {
        x: rect.x,
        y: rect.y,
        cols: rect.width,
        rows: rect.height,
    }
}

/// Core's `area` as a ratatui [`Rect`].
pub(crate) fn rect(area: Area) -> Rect {
    Rect::new(area.x, area.y, area.cols, area.rows)
}

/// The client's layout placed in the pane area `pane`.
pub(crate) fn arranged(client: &Client, pane: Rect) -> Arrangement {
    arrange(&client.host_layout(), area(pane))
}

/// The page bar's controls: each page drawn whole (its name, and whether it is the current one), then `+` if it fits.
fn page_bar(client: &Client, bar: Rect) -> Vec<(Rect, Target, DisplayText, bool)> {
    let layout = client.host_layout();
    let mut controls = Vec::new();
    let mut x = bar.x;
    let room = |x: u16, width: usize| usize::from(bar.right().saturating_sub(x)) >= width;
    for page in layout.pages() {
        let label = shown(&format!(
            " {} ",
            cut(shown(&page.name).as_str(), PAGE_NAME_MAX)
        ));
        let width = columns(label.as_str());
        // Keep room for `+` after it.
        if !room(x, width + 1 + columns(NEW_PAGE)) {
            break;
        }
        let w = u16::try_from(width).unwrap_or(u16::MAX);
        let current = page.id == layout.current();
        controls.push((
            Rect::new(x, bar.y, w, 1),
            Target::Page(page.id),
            label,
            current,
        ));
        x = x.saturating_add(w).saturating_add(1);
    }
    if room(x, columns(NEW_PAGE)) {
        controls.push((
            Rect::new(x, bar.y, 1, 1),
            Target::NewPage,
            shown(NEW_PAGE),
            false,
        ));
    }
    controls
}

/// Where a bordered pane's menu button is: at the right of its top edge, a blank on either side, inside its corners, if
/// it fits.
fn menu_button(place: &PanePlace) -> Option<Rect> {
    let width = u16::try_from(columns(MENU_BUTTON)).unwrap_or(u16::MAX);
    if !place.bordered || place.area.rows == 0 || place.area.cols < width + 4 {
        return None;
    }
    let x = place.area.x + place.area.cols - width - 2;
    Some(Rect::new(x, place.area.y, width, 1))
}

/// The focused pane's terminal: where its session is drawn, in the pane area `pane`.
pub(crate) fn focused_terminal(client: &Client, pane: Rect) -> Option<Rect> {
    let arranged = arranged(client, pane);
    let place = arranged.pane(client.focused_pane())?;
    place.terminal().map(rect)
}

/// What is under cell `col`, `row` of the pane area `pane`.
pub(crate) fn hit(client: &Client, pane: Rect, col: u16, row: u16) -> Option<Target> {
    let at = Position::new(col, row);
    let arranged = arranged(client, pane);
    if let Some(bar) = arranged.page_bar.map(rect)
        && bar.contains(at)
    {
        return page_bar(client, bar)
            .into_iter()
            .find(|(area, ..)| area.contains(at))
            .map(|(_, target, ..)| target);
    }
    for place in &arranged.panes {
        if menu_button(place).is_some_and(|b| b.contains(at)) {
            return Some(Target::PaneMenu(place.pane));
        }
    }
    if let Some(divider) = arranged
        .dividers
        .iter()
        .find(|d| d.cells.contains(col, row))
    {
        return Some(Target::Divider(divider.clone()));
    }
    let focused = client.focused_pane();
    let place = arranged.panes.iter().find(|p| p.area.contains(col, row))?;
    let in_terminal = place.terminal().is_some_and(|t| t.contains(col, row));
    if place.pane == focused && (in_terminal || !place.bordered) {
        return Some(Target::Terminal);
    }
    Some(Target::Pane(place.pane))
}

/// Draws the page bar and the current page's panes in the pane area `pane`.
pub(crate) fn render(client: &Client, screens: &Screens, pane: Rect, buf: &mut Buffer) {
    let arranged = arranged(client, pane);
    if let Some(bar) = arranged.page_bar.map(rect) {
        for (area, _, label, current) in page_bar(client, bar) {
            let style = if current {
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            draw::set(buf, area.x, area.y, &label, usize::from(area.width), style);
        }
    }
    let focused = client.focused_pane();
    for place in &arranged.panes {
        render_pane(client, screens, place, place.pane == focused, buf);
    }
}

/// What a pane shows, as this window sees it: its session (the selected one for a pane showing the selection) and the
/// machine it is on, else what it shows.
enum Shown {
    Session {
        id: SessionId,
        machine: Option<MachineId>,
    },
    /// A pane showing the selection, with nothing selected.
    NothingSelected,
    Empty,
    View(ViewKind),
}

fn shown_in(client: &Client, content: &Content) -> Shown {
    match content {
        Content::Selected => match client.selected() {
            Some(id) => Shown::Session {
                id: id.clone(),
                machine: machine_of(client, id),
            },
            None => Shown::NothingSelected,
        },
        Content::Session { machine, id } => Shown::Session {
            id: id.clone(),
            machine: machine
                .clone()
                .or_else(|| client.host_machine().cloned())
                .or_else(|| machine_of(client, id)),
        },
        Content::Empty => Shown::Empty,
        Content::View(kind) => Shown::View(kind.clone()),
    }
}

/// The machine listed session `id` is on.
fn machine_of(client: &Client, id: &SessionId) -> Option<MachineId> {
    client
        .sessions()
        .iter()
        .find(|s| &s.id == id)
        .map(|s| s.machine.clone())
}

/// Whether `machine` is one the client can't reach now: a peer that isn't connected, or one it doesn't know.
fn unreachable(client: &Client, machine: Option<&MachineId>, id: &SessionId) -> bool {
    if client.offline(id) {
        return true;
    }
    let Some(machine) = machine else {
        return false;
    };
    if client.host_machine() == Some(machine) {
        return false;
    }
    let groups = client.machines();
    match groups.iter().find(|g| &g.id == machine) {
        Some(group) => group.status != MachineStatus::Connected,
        None => !client.sessions().iter().any(|s| &s.id == id),
    }
}

fn view_name(kind: &ViewKind) -> String {
    match kind {
        ViewKind::Status => "Status".to_owned(),
        ViewKind::Machines => "Machines".to_owned(),
        ViewKind::PmPanel => "Project manager".to_owned(),
        ViewKind::Cleanup => "Cleanup".to_owned(),
        ViewKind::Archive => "Archive".to_owned(),
        ViewKind::Other(name) => name.clone(),
    }
}

fn render_pane(
    client: &Client,
    screens: &Screens,
    place: &PanePlace,
    focused: bool,
    buf: &mut Buffer,
) {
    let area = rect(place.area).intersection(buf.area);
    let shown_here = shown_in(client, &place.content);
    if place.bordered {
        frame(client, place, &shown_here, focused, buf);
    }
    let inner = rect(place.inner).intersection(buf.area);
    let Some(terminal) = place.terminal().map(rect) else {
        if place.bordered {
            placeholder(&["Too small"], inner, buf);
        }
        return;
    };
    let gray = Style::new().fg(Color::Gray);
    match &shown_here {
        Shown::Session { id, machine } => {
            let away = unreachable(client, machine.as_ref(), id);
            match screens.get(id) {
                Some(screen) => {
                    render_screen(screen, terminal, buf);
                    if away {
                        buf.set_style(terminal, Style::new().add_modifier(Modifier::DIM));
                    }
                }
                None if away => {
                    let name = machine
                        .as_ref()
                        .map_or("its machine", |m| client.machine_label(m));
                    let line = format!("{} can't be reached now", cut(name, NAME_MAX));
                    placeholder(&[&line], terminal, buf);
                    buf.set_style(terminal, gray.add_modifier(Modifier::DIM));
                }
                None => {}
            }
        }
        Shown::NothingSelected if !place.bordered && client.sessions().is_empty() => {
            crate::empty(client, area, buf);
        }
        Shown::NothingSelected if place.bordered => {
            placeholder(
                &["No session selected", "Click one in the sidebar."],
                terminal,
                buf,
            );
        }
        Shown::NothingSelected => {}
        Shown::Empty => placeholder(
            &[
                "Nothing here yet.",
                "Select a session, then choose Move session here in this pane's menu.",
            ],
            terminal,
            buf,
        ),
        Shown::View(kind) => {
            let line = format!("{} isn't available yet.", cut(&view_name(kind), NAME_MAX));
            placeholder(&[&line], terminal, buf);
        }
    }
}

/// A bordered pane's box: its border, its title on the top edge, and its menu button.
fn frame(client: &Client, place: &PanePlace, shown_here: &Shown, focused: bool, buf: &mut Buffer) {
    let area = rect(place.area).intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = if focused {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    draw::render(Block::bordered().border_style(border), area, buf);
    let button = menu_button(place);
    // The title runs from after the corner and a blank to before the button (or the far corner).
    let title_x = area.x.saturating_add(2);
    let end = button.map_or(area.right().saturating_sub(1), |b| b.x.saturating_sub(1));
    let room = usize::from(end.saturating_sub(title_x));
    if room > 0 {
        let title_style = if focused {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        let (name, machine) = title(client, shown_here);
        let full = match &machine {
            Some((_, label)) => format!("{name} · {label}"),
            None => name.clone(),
        };
        let text = shown(&cut(shown(&full).as_str(), room.saturating_sub(1)));
        // A blank on either side of it, over the border.
        let after = title_x.saturating_add(u16::try_from(columns(text.as_str())).unwrap_or(0));
        for x in [title_x - 1, after] {
            draw::set(buf, x, area.y, &shown(" "), 1, border);
        }
        draw::set(buf, title_x, area.y, &text, room, title_style);
        if let Some((id, label)) = &machine {
            let before = columns(shown(&format!("{name} · ")).as_str());
            let at = title_x.saturating_add(u16::try_from(before).unwrap_or(u16::MAX));
            let left = columns(text.as_str()).saturating_sub(before);
            if at < end && left > 0 {
                machine::tint(client, id, shown(label).as_str(), (at, area.y), left, buf);
            }
        }
    }
    if let Some(button) = button {
        let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        for x in [button.x - 1, button.right()] {
            draw::set(buf, x, button.y, &shown(" "), 1, border);
        }
        draw::set(
            buf,
            button.x,
            button.y,
            &shown(MENU_BUTTON),
            usize::from(button.width),
            style,
        );
    }
}

/// A pane's title: what it shows, and the machine of a session (its id and what to call it).
fn title(client: &Client, shown_here: &Shown) -> (String, Option<(MachineId, String)>) {
    match shown_here {
        Shown::Session { id, machine } => {
            let name = client
                .sessions()
                .iter()
                .find(|s| &s.id == id)
                .map_or_else(|| id.as_str().to_owned(), |s| s.name.clone());
            let machine = machine
                .as_ref()
                .map(|m| (m.clone(), client.machine_label(m).to_owned()));
            (name, machine)
        }
        Shown::NothingSelected => ("no session".to_owned(), None),
        Shown::Empty => ("empty".to_owned(), None),
        Shown::View(kind) => (view_name(kind), None),
    }
}

/// `lines`, centered in `area`, wrapped and clipped to it.
fn placeholder(lines: &[&str], area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let gray = Style::new().fg(Color::Gray);
    // Each line takes as many rows as it wraps to, at least.
    let rows: usize = lines
        .iter()
        .map(|l| columns(l).div_ceil(usize::from(area.width)).max(1))
        .sum();
    let lines: Vec<_> = lines.iter().map(|l| draw::line(shown(l), gray)).collect();
    let height = u16::try_from(rows).unwrap_or(u16::MAX).min(area.height);
    let at = Rect::new(
        area.x,
        area.y + (area.height - height) / 2,
        area.width,
        height,
    );
    draw::render(
        draw::paragraph(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        at,
        buf,
    );
}
