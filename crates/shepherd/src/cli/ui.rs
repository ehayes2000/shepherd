//! `shepherd ui …` (T50, decision 0022): read and change the host's screen layout, and see it drawn. Shapes in
//! `docs/cli.md`.
//!
//! Every change is one `_shepherd/ui/apply` to this machine's host (the layout is its own; peers' hosts aren't asked),
//! printed as the layout the host answers with. A screenshot draws the layout with the TUI's own renderer, headless, at
//! the size asked for: the window a user would see, sidebar included, with each session's terminal as the host's
//! snapshot of it shows it now. It sends nothing that changes anything: no session is resized, the shown page stays.

use std::io::Write;
use std::sync::Arc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use serde::Serialize;
use shepherd_core::client::Client;
use shepherd_core::client::panes::{Area, arrange};
use shepherd_core::display::display_safe;
use shepherd_core::layout::{
    Content, Layout, LayoutOp, Page, PageId, Pane, PaneId, Ratio, SplitDir, ViewKind,
};
use shepherd_core::wire::{ClientRequest, HostResult, UiApply, UiGet};
use shepherd_term::{MAX_COLS, MAX_ROWS, Screen};
use shepherd_tui::Screens;

use crate::client::ClientRuntime;
use shepherd_core::ports::{ConnId, HostLauncher, Link, LinkEventSink};

use super::{Args, Drive, request, safe, settle, target, write, write_json};

const USAGE: &str = "usage: shepherd ui get [--json]
       shepherd ui page new [NAME] [--json]
       shepherd ui page select|close <page> [--json]
       shepherd ui page rename <page> <name> [--json]
       shepherd ui split <pane> right|down [--ratio R] [--session S] [--json]
       shepherd ui put <pane> <session|view> [--json]
       shepherd ui close <pane> [--json]
       shepherd ui ratio <pane> R [--json]
       shepherd ui screenshot [--size WxH] [--page P] [--json]";

/// The window a screenshot draws without `--size`.
pub const SCREENSHOT_SIZE: (u16, u16) = (120, 40);

/// The size a screenshot's client makes each session's terminal at before it is fitted to its pane: a command-line
/// client never resizes a session, so the host's snapshot of it is replayed into a terminal at least as big as its PTY
/// is likely to be. A larger PTY is shown cut.
pub const SNAPSHOT_SIZE: (u16, u16) = (400, 200);

/// What `shepherd ui` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ui {
    Get,
    PageNew {
        name: Option<String>,
    },
    /// Each `page` is a page's id, or its name when no page has that id and one has that name.
    PageSelect {
        page: String,
    },
    PageRename {
        page: String,
        name: String,
    },
    PageClose {
        page: String,
    },
    Split {
        pane: PaneId,
        dir: SplitDir,
        ratio: Option<Ratio>,
        /// What the new pane shows: a session (by id or name), or nothing.
        session: Option<String>,
    },
    /// `what`: a view's name, `empty`, `selected`, or else a session's id or name.
    Put {
        pane: PaneId,
        what: String,
    },
    Close {
        pane: PaneId,
    },
    Ratio {
        pane: PaneId,
        ratio: Ratio,
    },
}

/// What `shepherd ui screenshot` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub cols: u16,
    pub rows: u16,
    /// A page to draw instead of the shown one: its id, or its name.
    pub page: Option<String>,
}

impl Default for Screenshot {
    fn default() -> Self {
        Self {
            cols: SCREENSHOT_SIZE.0,
            rows: SCREENSHOT_SIZE.1,
            page: None,
        }
    }
}

// ---- JSON shapes (docs/cli.md) ----

#[derive(Serialize)]
struct LayoutJson {
    current: u64,
    pages: Vec<PageJson>,
}

impl LayoutJson {
    fn of(client: &Client, layout: &Layout) -> Self {
        Self {
            current: layout.current().get(),
            pages: layout
                .pages()
                .iter()
                .map(|page| PageJson::of(client, page))
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct PageJson {
    id: u64,
    name: String,
    root: PaneJson,
}

impl PageJson {
    fn of(client: &Client, page: &Page) -> Self {
        Self {
            id: page.id.get(),
            name: page.name.clone(),
            root: PaneJson::of(client, &page.root),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum PaneJson {
    Split {
        id: u64,
        dir: &'static str,
        ratio: f64,
        a: Box<PaneJson>,
        b: Box<PaneJson>,
    },
    Leaf {
        id: u64,
        content: ContentJson,
    },
}

impl PaneJson {
    fn of(client: &Client, pane: &Pane) -> Self {
        match pane {
            Pane::Split {
                id,
                dir,
                ratio,
                a,
                b,
            } => Self::Split {
                id: id.get(),
                dir: dir_name(*dir),
                ratio: ratio_value(*ratio),
                a: Box::new(Self::of(client, a)),
                b: Box::new(Self::of(client, b)),
            },
            Pane::Leaf { id, content } => Self::Leaf {
                id: id.get(),
                content: ContentJson::of(client, content),
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ContentJson {
    Empty,
    Selected,
    Session {
        id: String,
        name: Option<String>,
        machine: String,
        machine_id: String,
    },
    View {
        view: String,
    },
}

impl ContentJson {
    fn of(client: &Client, content: &Content) -> Self {
        match content {
            Content::Empty => Self::Empty,
            Content::Selected => Self::Selected,
            Content::Session { machine, id } => {
                let machine = machine.clone().or_else(|| local_machine(client));
                let name = client
                    .sessions()
                    .iter()
                    .find(|s| &s.id == id)
                    .map(|s| s.name.clone());
                Self::Session {
                    id: id.to_string(),
                    name,
                    machine: machine
                        .as_ref()
                        .map(|m| client.machine_label(m).to_owned())
                        .unwrap_or_default(),
                    machine_id: machine.map(|m| m.to_string()).unwrap_or_default(),
                }
            }
            Content::View(view) => Self::View {
                view: view_name(view).to_owned(),
            },
        }
    }
}

#[derive(Serialize)]
struct PlacedJson {
    id: u64,
    /// What it shows: for a pane showing the selection, the session selected.
    content: ContentJson,
    /// It shows the selection.
    selected: bool,
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
}

fn dir_name(dir: SplitDir) -> &'static str {
    match dir {
        SplitDir::Horizontal => "right",
        SplitDir::Vertical => "down",
    }
}

fn ratio_value(ratio: Ratio) -> f64 {
    f64::from(ratio.permille()) / 1000.0
}

fn view_name(view: &ViewKind) -> &str {
    match view {
        ViewKind::Status => "status",
        ViewKind::Machines => "machines",
        ViewKind::PmPanel => "pm-panel",
        ViewKind::Cleanup => "cleanup",
        ViewKind::Archive => "archive",
        ViewKind::Other(name) => name,
    }
}

/// The views `put` can show, by name.
const VIEWS: [ViewKind; 5] = [
    ViewKind::Status,
    ViewKind::Machines,
    ViewKind::PmPanel,
    ViewKind::Cleanup,
    ViewKind::Archive,
];

fn local_machine(client: &Client) -> Option<shepherd_core::ids::MachineId> {
    client
        .machines()
        .into_iter()
        .find(|m| m.local)
        .map(|m| m.id)
}

// ---- the human form ----

/// One line per page (`page <id>: <name>`, the shown one marked), and under it its panes, indented by depth.
fn layout_text(client: &Client, layout: &Layout) -> String {
    let mut text = String::new();
    for page in layout.pages() {
        let shown = if page.id == layout.current() {
            " (shown)"
        } else {
            ""
        };
        text.push_str(&format!("page {}{shown}: {}\n", page.id, safe(&page.name)));
        pane_text(client, &page.root, 1, &mut text);
    }
    text
}

fn pane_text(client: &Client, pane: &Pane, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match pane {
        Pane::Split {
            id,
            dir,
            ratio,
            a,
            b,
        } => {
            out.push_str(&format!(
                "{indent}split {id} {} {}\n",
                dir_name(*dir),
                ratio_value(*ratio)
            ));
            pane_text(client, a, depth + 1, out);
            pane_text(client, b, depth + 1, out);
        }
        Pane::Leaf { id, content } => {
            out.push_str(&format!(
                "{indent}pane {id}: {}\n",
                content_text(client, content)
            ));
        }
    }
}

fn content_text(client: &Client, content: &Content) -> String {
    match ContentJson::of(client, content) {
        ContentJson::Empty => "empty".to_owned(),
        ContentJson::Selected => "selected".to_owned(),
        ContentJson::Session {
            id, name, machine, ..
        } => match name {
            Some(name) => format!(
                "session {} ({}) on {}",
                safe(&name),
                safe(&id),
                safe(&machine)
            ),
            None => format!("session {} on {}", safe(&id), safe(&machine)),
        },
        ContentJson::View { view } => format!("view {}", safe(&view)),
    }
}

// ---- layout commands ----

/// `shepherd ui <command>`: prints the host's layout (`get`), or applies one change to it and prints the layout the
/// host answers with. A change the host refuses is the error, and changes nothing.
pub fn ui(drive: &mut impl Drive, ui: &Ui, json: bool, out: &mut impl Write) -> Result<(), String> {
    settle(drive)?;
    let body = match layout_op(drive, ui)? {
        None => ClientRequest::UiGet(UiGet {}),
        Some(op) => ClientRequest::UiApply(UiApply { op }),
    };
    let layout = match request(drive, None, body)? {
        HostResult::Layout(layout) => layout,
        other => return Err(format!("the host answered something else: {other:?}")),
    };
    let client = drive.client();
    if json {
        #[derive(Serialize)]
        struct Shown {
            layout: LayoutJson,
        }
        return write_json(
            out,
            &Shown {
                layout: LayoutJson::of(client, &layout),
            },
        );
    }
    write(out, &layout_text(client, &layout))
}

/// The op `ui` asks for, with its pages and sessions named as the host knows them; none for `get`.
fn layout_op(drive: &impl Drive, ui: &Ui) -> Result<Option<LayoutOp>, String> {
    let layout = drive.client().host_layout();
    Ok(Some(match ui {
        Ui::Get => return Ok(None),
        Ui::PageNew { name } => LayoutOp::PageNew {
            name: name.clone(),
            content: Content::Empty,
        },
        Ui::PageSelect { page } => LayoutOp::PageSelect {
            page: resolve_page(&layout, page)?,
        },
        Ui::PageRename { page, name } => LayoutOp::PageRename {
            page: resolve_page(&layout, page)?,
            name: name.clone(),
        },
        Ui::PageClose { page } => LayoutOp::PageClose {
            page: resolve_page(&layout, page)?,
        },
        Ui::Split {
            pane,
            dir,
            ratio,
            session,
        } => LayoutOp::Split {
            pane: *pane,
            dir: *dir,
            ratio: ratio.unwrap_or_default(),
            content: match session {
                None => Content::Empty,
                Some(text) => session_content(drive, text)?,
            },
        },
        Ui::Put { pane, what } => LayoutOp::Put {
            pane: *pane,
            content: put_content(drive, what)?,
        },
        Ui::Close { pane } => LayoutOp::Close { pane: *pane },
        Ui::Ratio { pane, ratio } => LayoutOp::Ratio {
            pane: *pane,
            ratio: *ratio,
        },
    }))
}

/// The session `text` names, pinned: this machine's own by id alone, a peer's with its machine.
fn session_content(drive: &impl Drive, text: &str) -> Result<Content, String> {
    let session = target(drive, text)?;
    Ok(if drive.client().is_home(None, &session.id) {
        Content::session(session.id)
    } else {
        Content::peer_session(session.machine, session.id)
    })
}

/// What `put` shows: a view by name, nothing (`empty`), the window's selection (`selected`), or a session.
fn put_content(drive: &impl Drive, what: &str) -> Result<Content, String> {
    if what == "empty" {
        return Ok(Content::Empty);
    }
    if what == "selected" {
        return Ok(Content::Selected);
    }
    if let Some(view) = VIEWS.iter().find(|v| view_name(v) == what) {
        return Ok(Content::View(view.clone()));
    }
    session_content(drive, what)
}

/// The page `text` names: by id, or by name when no page has that id and exactly one has that name.
fn resolve_page(layout: &Layout, text: &str) -> Result<PageId, String> {
    let pages = layout.pages();
    if let Ok(id) = text.parse::<u64>()
        && let Some(page) = pages.iter().find(|p| p.id.get() == id)
    {
        return Ok(page.id);
    }
    let named: Vec<&Page> = pages.iter().filter(|p| p.name == text).collect();
    let list = |pages: &[&Page]| {
        pages
            .iter()
            .map(|p| format!("{} ({})", p.id, safe(&p.name)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match named[..] {
        [page] => Ok(page.id),
        [] => Err(format!(
            "no page `{}`: the pages are {}",
            safe(text),
            list(&pages.iter().collect::<Vec<_>>())
        )),
        _ => Err(format!(
            "more than one page is called `{}`: {}",
            safe(text),
            list(&named)
        )),
    }
}

// ---- screenshot ----

/// Readies a client runtime for [`screenshot`], before it starts: each session's terminal is made at [`SNAPSHOT_SIZE`],
/// so the host's snapshot of it replays whole.
pub fn prepare<L, C, H>(runtime: &mut ClientRuntime<L, C, H>)
where
    L: Link,
    C: FnMut(Arc<dyn LinkEventSink>) -> Result<(L, ConnId), String>,
    H: HostLauncher,
{
    runtime.set_blank_screen_size(SNAPSHOT_SIZE);
}

/// `shepherd ui screenshot`: the window a user would see at `shot`'s size, drawn by the TUI from this client's state
/// (its sidebar selects what a newly opened window does: the first session it lists), as text. A `Selected` pane
/// shows that session. With `json`, also where each pane is.
pub fn screenshot(
    drive: &mut impl Drive,
    shot: &Screenshot,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    settle(drive)?;
    let client = drive.client();
    let host_layout = client.host_layout();
    let page = match &shot.page {
        None => host_layout.current(),
        Some(page) => resolve_page(&host_layout, page)?,
    };
    // The host's layout as the window would show it with `page` selected, and its `Selected` panes as this one does.
    let drawn = host_layout
        .apply(&LayoutOp::PageSelect { page }, |_| true)
        .map_err(|e| e.to_string())?;
    let window = Rect::new(0, 0, shot.cols, shot.rows);
    let pane = shepherd_tui::layout(window).pane;
    let layout = drawn.resolve_selected(&selected_content(client));
    let arranged = arrange(
        &layout,
        Area {
            x: pane.x,
            y: pane.y,
            cols: pane.width,
            rows: pane.height,
        },
    );
    let screens = fitted(drive.screens(), &arranged);
    let mut terminal =
        Terminal::new(TestBackend::new(shot.cols, shot.rows)).map_err(|e| e.to_string())?;
    terminal
        .draw(|frame| shepherd_tui::render_page(client, &screens, &host_layout, page, frame))
        .map_err(|e| e.to_string())?;
    let lines = lines_of(terminal.backend().buffer());
    if json {
        #[derive(Serialize)]
        struct Shot {
            cols: u16,
            rows: u16,
            page: u64,
            panes: Vec<PlacedJson>,
            lines: Vec<String>,
        }
        let panes = arranged
            .panes
            .iter()
            .map(|p| PlacedJson {
                id: p.pane.get(),
                content: ContentJson::of(client, &p.content),
                selected: matches!(
                    host_layout.pane(p.pane),
                    Some(Pane::Leaf {
                        content: Content::Selected,
                        ..
                    })
                ),
                x: p.area.x,
                y: p.area.y,
                cols: p.area.cols,
                rows: p.area.rows,
            })
            .collect();
        return write_json(
            out,
            &Shot {
                cols: shot.cols,
                rows: shot.rows,
                page: layout.current().get(),
                panes,
                lines,
            },
        );
    }
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    write(out, &text)
}

/// What a pane showing the selection shows in this client: its selected session, pinned to its machine when a peer has
/// it, or nothing.
fn selected_content(client: &Client) -> Content {
    match client.selected() {
        None => Content::Empty,
        Some(id) if client.is_home(None, id) => Content::session(id.clone()),
        Some(id) => client
            .sessions()
            .iter()
            .find(|s| &s.id == id)
            .map_or(Content::Empty, |s| {
                Content::peer_session(s.machine.clone(), id.clone())
            }),
    }
}

/// Each session's terminal as a pane showing it draws it: the host's snapshot of it, replayed whole, then cut to the
/// first such pane's terminal (the session itself is never resized). Rows are cut from the top as a terminal made
/// shorter cuts them, keeping the cursor's row; columns are cut from the right, never rewrapped, so what the session
/// drew stays where it drew it. A session in no pane is left out.
fn fitted(screens: &Screens, arranged: &shepherd_core::client::panes::Arrangement) -> Screens {
    let mut fitted = Screens::new();
    for place in &arranged.panes {
        let (Content::Session { id, .. }, Some(terminal)) = (&place.content, place.terminal())
        else {
            continue;
        };
        let Some(screen) = screens.get(id) else {
            continue;
        };
        if fitted.contains_key(id) {
            continue;
        }
        let (cols, rows) = screen.size();
        let mut copy = Screen::new(cols, rows);
        copy.feed(&screen.snapshot_ansi());
        // Only the rows: rewrapping to fewer columns would also count the blank rows below the cursor as lines, and
        // push what is drawn above them out of sight. The renderer cuts the columns.
        copy.resize(cols, terminal.rows);
        fitted.insert(id.clone(), copy);
    }
    fitted
}

/// The text of each row of `buf`, display-safe, without trailing blanks. The cell a wide character covers is skipped.
fn lines_of(buf: &Buffer) -> Vec<String> {
    (buf.area.top()..buf.area.bottom())
        .map(|y| {
            let mut line = String::new();
            let mut skip: u16 = 0;
            for x in buf.area.left()..buf.area.right() {
                let cell = &buf[(x, y)];
                if skip == 0 {
                    let symbol = display_safe(cell.symbol(), 1).into_string();
                    line.push_str(if symbol.is_empty() { " " } else { &symbol });
                }
                skip = skip.max(cell.cell_width()).saturating_sub(1);
            }
            line.trim_end().to_owned()
        })
        .collect()
}

// ---- argv ----

fn pane_arg(text: &str) -> Option<PaneId> {
    text.parse().ok().map(PaneId::new)
}

/// A ratio as given: a fraction from 0 to 1 (clamped by the host to 0.05..0.95).
fn ratio_arg(text: &str) -> Option<Ratio> {
    let value: f64 = text.parse().ok()?;
    if !(0.0..=1.0).contains(&value) {
        return None;
    }
    let permille = (value * 1000.0).round();
    Some(Ratio::from_permille(permille as u16))
}

/// `WxH`, each from 1 to the most a terminal holds.
fn size_arg(text: &str) -> Option<(u16, u16)> {
    let (cols, rows) = text.split_once('x')?;
    let (cols, rows): (u16, u16) = (cols.parse().ok()?, rows.parse().ok()?);
    ((1..=MAX_COLS).contains(&cols) && (1..=MAX_ROWS).contains(&rows)).then_some((cols, rows))
}

/// What a `shepherd ui` command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Layout(Ui),
    Screenshot(Screenshot),
}

/// Parses `shepherd ui <args>`: the command and `--json`.
pub fn parse(args: &[String]) -> Option<(Command, bool)> {
    let parsed = Args::parse(
        args,
        &["--ratio", "--session", "--size", "--page"],
        &["--json"],
    )?;
    let json = parsed.switch("--json");
    let words: Vec<&str> = parsed.positional.iter().map(String::as_str).collect();
    let flags = |allowed: &[&str]| {
        ["--ratio", "--session", "--size", "--page"]
            .iter()
            .all(|f| allowed.contains(f) || parsed.value(f).is_none())
    };
    let command = match words[..] {
        ["get"] if flags(&[]) => Command::Layout(Ui::Get),
        ["page", "new"] if flags(&[]) => Command::Layout(Ui::PageNew { name: None }),
        ["page", "new", name] if flags(&[]) => Command::Layout(Ui::PageNew {
            name: Some(name.to_owned()),
        }),
        ["page", "select", page] if flags(&[]) => Command::Layout(Ui::PageSelect {
            page: page.to_owned(),
        }),
        ["page", "rename", page, name] if flags(&[]) => Command::Layout(Ui::PageRename {
            page: page.to_owned(),
            name: name.to_owned(),
        }),
        ["page", "close", page] if flags(&[]) => Command::Layout(Ui::PageClose {
            page: page.to_owned(),
        }),
        ["split", pane, dir] if flags(&["--ratio", "--session"]) => Command::Layout(Ui::Split {
            pane: pane_arg(pane)?,
            dir: match dir {
                "right" => SplitDir::Horizontal,
                "down" => SplitDir::Vertical,
                _ => return None,
            },
            ratio: match parsed.value("--ratio") {
                None => None,
                Some(r) => Some(ratio_arg(r)?),
            },
            session: parsed.value("--session").map(str::to_owned),
        }),
        ["put", pane, what] if flags(&[]) => Command::Layout(Ui::Put {
            pane: pane_arg(pane)?,
            what: what.to_owned(),
        }),
        ["close", pane] if flags(&[]) => Command::Layout(Ui::Close {
            pane: pane_arg(pane)?,
        }),
        ["ratio", pane, ratio] if flags(&[]) => Command::Layout(Ui::Ratio {
            pane: pane_arg(pane)?,
            ratio: ratio_arg(ratio)?,
        }),
        ["screenshot"] if flags(&["--size", "--page"]) => {
            let (cols, rows) = match parsed.value("--size") {
                None => SCREENSHOT_SIZE,
                Some(size) => size_arg(size)?,
            };
            Command::Screenshot(Screenshot {
                cols,
                rows,
                page: parsed.value("--page").map(str::to_owned),
            })
        }
        _ => return None,
    };
    Some((command, json))
}

/// `shepherd ui <args>`.
pub fn main(args: &[String]) -> Result<(), String> {
    let (command, json) = parse(args).ok_or(USAGE)?;
    let stdout = &mut std::io::stdout();
    match command {
        Command::Layout(op) => ui(&mut super::connect()?, &op, json, stdout),
        Command::Screenshot(shot) => {
            let mut runtime = crate::client::socket_runtime(&crate::peers::socket()?)
                .map_err(|e| e.to_string())?;
            prepare(&mut runtime);
            let mut runtime = super::started(runtime);
            screenshot(&mut runtime, &shot, json, stdout)
        }
    }
}
