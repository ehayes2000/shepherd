//! Random sequences of what a user does (clicks on drawn words, keys, the wheel, pastes, resizes down to 1x1, dragging
//! dividers) and of what the host does (including layout ops from elsewhere: pages, splits, closes, what panes show,
//! ratios, and subagents starting, blocking, finishing and dropping at a turn's end) never panic and keep these invariants after every step. Layout ops the client asks for are done by the host
//! after each step.
//!
//! - every button drawn whole is hittable: a click on its first cell finds a control, not nothing, the popup's body,
//!   the backdrop or the terminal, and a subagent badge (`▸N` / `▾N`) finds its session's badge;
//! - in the narrow sidebar, the glyph of a session with subagents and each subagent's branch toggle its tree;
//! - the selected session's row stays visible: unless the user has wheeled the list (or the host has listed a new
//!   session above it) since the selection or the window last changed, some cell of the sidebar hits it (while no
//!   popup covers it and the sidebar has room for a row), however the subagent trees above it change;
//! - the focused pane is a pane of the current page, and the session it shows (if it has room for a terminal) was last
//!   sized by this window to exactly that terminal;
//! - a status page's rows hit listed sessions, and after a key that moves the focused status table's selection (or
//!   regroups it), the selected session's row is visible.
//!
//! Seeded and bounded by count (cases x steps), not time. A failure prints its seed and its (shrunk) steps. Run another
//! seed with `PROPTEST_RNG_SEED=<n> cargo test -p shepherd-tui --test ui invariants`.

use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestError, TestRunner};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Position;
use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::client::panes::{Area, arrange};
use shepherd_core::client::{Focus, Popup};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{Content, LayoutOp, Pane, PaneId, Ratio, SplitDir, ViewKind};
use shepherd_core::ports::pty::PtySize;
use shepherd_tui::Target;
use shepherd_tui::layout::{layout, pane_size, sidebar_rows};

use crate::harness::Ui;

/// The seed unless `PROPTEST_RNG_SEED` says otherwise.
const SEED: u64 = 0x5eed_0045;
const CASES: u32 = 96;
const MAX_STEPS: usize = 40;

const KEYS: &[(KeyCode, KeyModifiers)] = &[
    (KeyCode::Enter, KeyModifiers::NONE),
    (KeyCode::Esc, KeyModifiers::NONE),
    (KeyCode::Backspace, KeyModifiers::NONE),
    (KeyCode::Tab, KeyModifiers::NONE),
    (KeyCode::Up, KeyModifiers::NONE),
    (KeyCode::Down, KeyModifiers::NONE),
    (KeyCode::PageDown, KeyModifiers::NONE),
    (KeyCode::PageUp, KeyModifiers::NONE),
    (KeyCode::Char('b'), KeyModifiers::CONTROL),
    (KeyCode::Char('c'), KeyModifiers::CONTROL),
];

const AGENT_TYPES: &[&str] = &[
    "builder",
    "Explore",
    "",
    "a-type-much-longer-than-the-sidebar",
    "日本語",
];

const NAMES: &[&str] = &[
    "fix-login",
    "docs",
    "a-name-much-longer-than-the-sidebar",
    "日本語",
    "x",
];

#[derive(Debug, Clone)]
enum Step {
    /// A left click on the first cell of a drawn word.
    Click(Index),
    /// A right click on the first cell of a drawn word.
    RightClick(Index),
    /// A left click anywhere, the cell taken modulo the window.
    ClickAt(u16, u16),
    /// The wheel anywhere, the cell taken modulo the window.
    Scroll(u16, u16, i32),
    Key(Index),
    Type(char),
    Paste(String),
    Resize(u16, u16),
    /// The host makes a new session.
    HostNew,
    /// The host renames a session.
    HostRename(Index, Index),
    /// A session's PTY exits.
    HostExit(Index),
    /// Something else (another window, the CLI) changes the host's layout.
    HostLayout(LayoutStep),
    /// The mouse dragged from one cell to another, each taken modulo the window.
    Drag(u16, u16, u16, u16),
    /// A session's agent starts a subagent of a type.
    SubagentStart(Index, Index),
    /// One of a session's subagents (the index taken modulo the subagents it has had) blocks on the user.
    SubagentBlock(Index, u8),
    /// One of a session's subagents finishes.
    SubagentFinish(Index, u8),
    /// A session's agent ends its turn, dropping its subagents.
    TurnEnd(Index),
    /// `ctrl-b s`: the status page.
    OpenStatus,
}

#[derive(Debug, Clone)]
enum LayoutStep {
    /// Split a pane of the current page, side by side (`true`) or stacked, the new half showing a content.
    Split(Index, bool, Index),
    Close(Index),
    Put(Index, Index),
    PageNew,
    PageSelect(Index),
    PageClose(Index),
    Ratio(Index, u16),
}

fn layout_step() -> impl Strategy<Value = LayoutStep> {
    prop_oneof![
        3 => (any::<Index>(), any::<bool>(), any::<Index>()).prop_map(|(p, h, c)| LayoutStep::Split(p, h, c)),
        2 => any::<Index>().prop_map(LayoutStep::Close),
        2 => (any::<Index>(), any::<Index>()).prop_map(|(p, c)| LayoutStep::Put(p, c)),
        1 => Just(LayoutStep::PageNew),
        1 => any::<Index>().prop_map(LayoutStep::PageSelect),
        1 => any::<Index>().prop_map(LayoutStep::PageClose),
        1 => (any::<Index>(), 0u16..=1000).prop_map(|(p, r)| LayoutStep::Ratio(p, r)),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => any::<Index>().prop_map(Step::Click),
        2 => any::<Index>().prop_map(Step::RightClick),
        1 => (any::<u16>(), any::<u16>()).prop_map(|(x, y)| Step::ClickAt(x, y)),
        2 => (any::<u16>(), any::<u16>(), -8i32..8).prop_map(|(x, y, by)| Step::Scroll(x, y, by)),
        3 => any::<Index>().prop_map(Step::Key),
        2 => prop::sample::select(vec!['n', 'y', 'q', '?', 'r', 'x', 'd', '日', 'g', 'j', 'k']).prop_map(Step::Type),
        1 => Just(Step::OpenStatus),
        1 => "[a-z/ ]{0,6}(\n[a-z]{0,3})?".prop_map(Step::Paste),
        2 => (1u16..=120, 1u16..=40).prop_map(|(w, h)| Step::Resize(w, h)),
        1 => Just(Step::HostNew),
        1 => (any::<Index>(), any::<Index>()).prop_map(|(s, n)| Step::HostRename(s, n)),
        1 => any::<Index>().prop_map(Step::HostExit),
        3 => layout_step().prop_map(Step::HostLayout),
        1 => (any::<u16>(), any::<u16>(), any::<u16>(), any::<u16>())
            .prop_map(|(x0, y0, x1, y1)| Step::Drag(x0, y0, x1, y1)),
        3 => (any::<Index>(), any::<Index>()).prop_map(|(s, t)| Step::SubagentStart(s, t)),
        1 => (any::<Index>(), any::<u8>()).prop_map(|(s, a)| Step::SubagentBlock(s, a)),
        1 => (any::<Index>(), any::<u8>()).prop_map(|(s, a)| Step::SubagentFinish(s, a)),
        1 => any::<Index>().prop_map(Step::TurnEnd),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    sessions: usize,
    size: (u16, u16),
    steps: Vec<Step>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        0usize..20,
        (1u16..=120, 1u16..=40),
        prop::collection::vec(step(), 0..=MAX_STEPS),
    )
        .prop_map(|(sessions, size, steps)| Case {
            sessions,
            size,
            steps,
        })
}

/// What the run tracks between steps.
struct Run {
    ui: Ui,
    /// How many sessions the host has made, for fresh ids.
    made: usize,
    /// How many subagents the host has started, for fresh ids.
    subagents: u8,
    /// The sidebar's scroll may have left the selection since the selection or the window last changed: the user
    /// wheeled it, or the host listed a session above it.
    unfitted: bool,
    /// The last step was a key that moved the focused status table's selection or regrouped it.
    status_moved: bool,
}

impl Run {
    fn session(&self, at: &Index) -> Option<String> {
        let sessions = self.ui.client().sessions();
        (!sessions.is_empty()).then(|| sessions[at.index(sessions.len())].id.as_str().to_owned())
    }

    fn word(&self, at: &Index) -> Option<Position> {
        let words = self.ui.words();
        (!words.is_empty()).then(|| words[at.index(words.len())].0)
    }

    fn cell(&self, x: u16, y: u16) -> Position {
        let window = self.ui.window();
        Position::new(x % window.width, y % window.height)
    }

    fn apply(&mut self, step: &Step) {
        let before = self.ui.client().selected().cloned();
        let popup = self.ui.client().popup().is_some();
        let on_status =
            !popup && self.ui.client().focus() == Focus::Terminal && self.status_focused();
        self.status_moved = false;
        match step {
            Step::Click(at) => {
                if let Some(p) = self.word(at) {
                    self.ui.click_at(p.x, p.y);
                }
            }
            Step::RightClick(at) => {
                if let Some(p) = self.word(at) {
                    self.ui.right_click_at(p.x, p.y);
                }
            }
            Step::ClickAt(x, y) => {
                let p = self.cell(*x, *y);
                self.ui.click_at(p.x, p.y);
            }
            Step::Scroll(x, y, by) => {
                let p = self.cell(*x, *y);
                let over_sidebar = layout(self.ui.window()).sidebar.contains(p);
                self.ui.scroll_at(p.x, p.y, *by);
                if !popup && over_sidebar && *by != 0 {
                    self.unfitted = true;
                }
            }
            Step::Key(at) => {
                let (code, modifiers) = KEYS[at.index(KEYS.len())];
                self.ui.key_with(code, modifiers);
                let moves = matches!(
                    code,
                    KeyCode::Up | KeyCode::Down | KeyCode::PageDown | KeyCode::PageUp
                );
                self.status_moved = on_status && moves && modifiers == KeyModifiers::NONE;
            }
            Step::Type(ch) => {
                self.ui.type_text(&ch.to_string());
                self.status_moved = on_status && matches!(ch, 'g' | 'j' | 'k');
            }
            Step::OpenStatus => {
                self.ui.ctrl('b').key(KeyCode::Char('s'));
            }
            Step::Paste(text) => {
                self.ui.paste(text);
            }
            Step::Resize(w, h) => {
                self.ui.resize(*w, *h);
                self.unfitted = false;
            }
            Step::HostNew => {
                self.made += 1;
                let id = format!("h{}", self.made);
                self.ui.on_host(|h| {
                    h.new_session(&id);
                });
                // Known gap (reported with T45): a session the host makes is listed above the selection without
                // scrolling it back into view, which can push it off the bottom of the sidebar.
                self.unfitted = true;
            }
            Step::HostRename(session, name) => {
                if let Some(id) = self.session(session) {
                    let name = NAMES[name.index(NAMES.len())];
                    self.ui.on_host(|h| {
                        h.rename(&id, name);
                    });
                }
            }
            Step::HostExit(session) => {
                if let Some(id) = self.session(session) {
                    self.ui.on_host(|h| {
                        h.pty_exited(&id, Some(0));
                    });
                }
            }
            Step::HostLayout(step) => {
                if let Some(op) = self.layout_op(step) {
                    self.ui.host_layout(op);
                }
            }
            Step::Drag(x0, y0, x1, y1) => {
                let (from, to) = (self.cell(*x0, *y0), self.cell(*x1, *y1));
                self.ui.drag(from, to);
            }
            Step::SubagentStart(session, kind) => {
                if let Some(id) = self.session(session) {
                    self.subagents = self.subagents.wrapping_add(1);
                    let params = json!({
                        "id": format!("a{}", self.subagents),
                        "agent_type": AGENT_TYPES[kind.index(AGENT_TYPES.len())],
                        "task": "look into it",
                    });
                    self.subagent_frame(&id, "_shepherd/subagent/started", params);
                }
            }
            Step::SubagentBlock(session, which) => {
                if let Some(id) = self.session(session) {
                    let params = json!({
                        "id": self.subagent_id(*which),
                        "kind": "permission",
                        "text": "Bash(ls)",
                        "agent_type": "builder",
                    });
                    self.subagent_frame(&id, "_shepherd/subagent/blocked", params);
                }
            }
            Step::SubagentFinish(session, which) => {
                if let Some(id) = self.session(session) {
                    let params = json!({ "id": self.subagent_id(*which) });
                    self.subagent_frame(&id, "_shepherd/subagent/finished", params);
                }
            }
            Step::TurnEnd(session) => {
                if let Some(id) = self.session(session) {
                    self.subagent_frame(&id, "_shepherd/agent/idle", json!({}));
                }
            }
        }
        self.ui.apply_layout_ops();
        if self.ui.client().selected() != before.as_ref() {
            self.unfitted = false;
        }
    }

    /// One of the subagents started so far (`a1` when none has).
    fn subagent_id(&self, which: u8) -> String {
        format!("a{}", which % self.subagents.max(1) + 1)
    }

    /// Appends a subagent (or turn) frame to session `id`'s log as a hook would. An expanded tree above the selection
    /// that changes size moves it, and the client scrolls it back into view, so this never unfits the list.
    fn subagent_frame(&mut self, id: &str, method: &str, params: serde_json::Value) {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
        self.ui.on_host(|h| {
            h.append(Some(id), source, frame);
        });
    }

    /// What `step` asks of the host's layout, on the panes and pages it has now.
    fn layout_op(&self, step: &LayoutStep) -> Option<LayoutOp> {
        let layout = self.ui.client().host_layout();
        let mut leaves = Vec::new();
        let mut splits = Vec::new();
        walk(&layout.current_page().root, &mut leaves, &mut splits);
        let pages: Vec<_> = layout.pages().iter().map(|p| p.id).collect();
        let leaf = |at: &Index| leaves[at.index(leaves.len())];
        let page = |at: &Index| pages[at.index(pages.len())];
        Some(match step {
            LayoutStep::Split(at, side, content) => LayoutOp::Split {
                pane: leaf(at),
                dir: if *side {
                    SplitDir::Horizontal
                } else {
                    SplitDir::Vertical
                },
                ratio: Ratio::default(),
                content: self.content(content),
            },
            LayoutStep::Close(at) => LayoutOp::Close { pane: leaf(at) },
            LayoutStep::Put(at, content) => LayoutOp::Put {
                pane: leaf(at),
                content: self.content(content),
            },
            LayoutStep::PageNew => LayoutOp::PageNew {
                name: None,
                content: Content::Selected,
            },
            LayoutStep::PageSelect(at) => LayoutOp::PageSelect { page: page(at) },
            LayoutStep::PageClose(at) => LayoutOp::PageClose { page: page(at) },
            LayoutStep::Ratio(at, permille) => {
                if splits.is_empty() {
                    return None;
                }
                LayoutOp::Ratio {
                    pane: splits[at.index(splits.len())],
                    ratio: Ratio::from_permille(*permille),
                }
            }
        })
    }

    /// Something for a pane to show: the selection, nothing, a view, a listed session or a peer's.
    fn content(&self, at: &Index) -> Content {
        let mut choices = vec![
            Content::Selected,
            Content::Empty,
            Content::View(ViewKind::Status),
            Content::peer_session(MachineId::new("m_far"), SessionId::new("p1")),
        ];
        choices.extend(
            self.ui
                .client()
                .sessions()
                .iter()
                .map(|s| Content::session(s.id.clone())),
        );
        choices[at.index(choices.len())].clone()
    }

    /// The invariants, or what broke them.
    fn check(&self) -> Result<(), String> {
        self.buttons_are_hittable()?;
        self.narrow_trees_toggle()?;
        self.selection_is_visible()?;
        self.focused_pane_is_sized()?;
        self.status_rows_hit_listed_sessions()?;
        self.status_selection_is_visible()
    }

    /// Whether the focused pane shows the status page.
    fn status_focused(&self) -> bool {
        let client = self.ui.client();
        matches!(
            client.host_layout().pane(client.focused_pane()),
            Some(Pane::Leaf {
                content: Content::View(ViewKind::Status),
                ..
            })
        )
    }

    /// Each session a line of a status page hits as a status row (a row is a whole line of its table, so one column
    /// of each line is enough).
    fn status_rows(&self) -> Vec<SessionId> {
        let pane = layout(self.ui.window()).pane;
        let view = Area {
            x: pane.x,
            y: pane.y,
            cols: pane.width,
            rows: pane.height,
        };
        let arranged = arrange(&self.ui.client().host_layout(), view);
        arranged
            .panes
            .iter()
            .filter(|p| p.content == Content::View(ViewKind::Status))
            .filter_map(|p| p.terminal())
            .flat_map(|t| (t.y..t.y + t.rows).map(move |y| (t.x.saturating_add(2), y)))
            .filter_map(|(x, y)| match self.ui.hit_at(x, y) {
                Some(Target::StatusRow { session, .. }) => Some(session),
                _ => None,
            })
            .collect()
    }

    fn status_rows_hit_listed_sessions(&self) -> Result<(), String> {
        let sessions = self.ui.client().sessions();
        match self
            .status_rows()
            .into_iter()
            .find(|id| !sessions.iter().any(|s| &s.id == id))
        {
            Some(id) => Err(format!("a status row hits {id}, which isn't listed")),
            None => Ok(()),
        }
    }

    fn status_selection_is_visible(&self) -> Result<(), String> {
        let client = self.ui.client();
        if !self.status_moved || client.popup().is_some() || !self.status_focused() {
            return Ok(());
        }
        let Some(selected) = client.status_selected() else {
            return Ok(());
        };
        let rows = self.status_rows();
        // A table too small for any line (or a pane too small for a table) shows no rows at all.
        if rows.is_empty() || rows.contains(selected) {
            return Ok(());
        }
        Err(format!(
            "the status table's selection {selected} has no visible row"
        ))
    }

    fn focused_pane_is_sized(&self) -> Result<(), String> {
        let client = self.ui.client();
        let host_layout = client.host_layout();
        let focused = client.focused_pane();
        match host_layout.current_page().root.find(focused) {
            Some(Pane::Leaf { .. }) => {}
            other => return Err(format!("the focused pane {focused} is {other:?}")),
        }
        let window = self.ui.window();
        let pane = layout(window).pane;
        let size = pane_size(window.width, window.height);
        // The client arranges the layout at the pane area's size, never under the emulator's smallest screen.
        if (pane.width, pane.height) != (size.cols, size.rows) {
            return Ok(());
        }
        let Some(session) = client.focused_session() else {
            return Ok(());
        };
        if !client.sessions().iter().any(|s| s.id == session) {
            return Ok(());
        }
        let view = Area {
            x: 0,
            y: 0,
            cols: size.cols,
            rows: size.rows,
        };
        let arranged = arrange(&client.layout(), view);
        let Some(terminal) = arranged.pane(focused).and_then(|p| p.terminal()) else {
            return Ok(());
        };
        let wanted = PtySize {
            cols: terminal.cols,
            rows: terminal.rows,
        };
        match client.session_size(&session) {
            Some(given) if given == wanted => Ok(()),
            given => Err(format!(
                "the focused session {session} was last sized {given:?}, not to its pane's {wanted:?}"
            )),
        }
    }

    /// In the narrow sidebar, which has no badges, the glyph of a session with subagents and the branch of each
    /// subagent's line toggle the tree.
    fn narrow_trees_toggle(&self) -> Result<(), String> {
        let client = self.ui.client();
        let layout = layout(self.ui.window());
        if !layout.collapsed || client.popup().is_some() || layout.sidebar.width < 3 {
            return Ok(());
        }
        let x = layout.sidebar.x;
        for y in layout.sidebar.top()..layout.sidebar.bottom() {
            let branch = matches!(self.ui.buffer()[(x, y)].symbol(), "├" | "└");
            if branch && !matches!(self.ui.hit_at(x, y), Some(Target::Subagents(_))) {
                return Err(format!(
                    "the branch at ({x}, {y}) hits {:?}",
                    self.ui.hit_at(x, y)
                ));
            }
            let glyph = !self.ui.buffer()[(x + 1, y)].symbol().trim().is_empty();
            if let (true, Some(Target::Session(id))) = (glyph, self.ui.hit_at(x + 1, y)) {
                let has_tree = client
                    .sessions()
                    .iter()
                    .any(|s| s.id == id && !s.subagents.is_empty());
                if has_tree {
                    return Err(format!(
                        "the glyph of {id}, which has subagents, at ({}, {y}) only selects it",
                        x + 1
                    ));
                }
            }
        }
        Ok(())
    }

    fn buttons_are_hittable(&self) -> Result<(), String> {
        let popup = self.ui.client().popup().is_some();
        for (at, word) in self.ui.words() {
            // A pane's menu button is under the popup's backdrop like everything else outside it.
            if popup && word.starts_with("[≡") {
                continue;
            }
            // Help's prose names `[≡]`; in a narrow help it is cut at the border, leaving `[│`.
            let help = self.ui.client().popup() == Some(&Popup::Help);
            if help && word.starts_with("[│") {
                continue;
            }
            let bracketed = word.starts_with('[') && word.ends_with(']') && word.len() > 2;
            // `[Start session]` and the like are two words; the first of them starts the button.
            let opens_button = word.starts_with('[') && !word.contains(']') && word.len() > 1;
            let sidebar_button = !popup && matches!(word.as_str(), "Detach" | "×" | "⊞");
            let in_sidebar = layout(self.ui.window()).sidebar.contains(at);
            let badge = !popup
                && in_sidebar
                && word.starts_with(['▸', '▾'])
                && word.chars().skip(1).all(|c| c.is_ascii_digit());
            if badge && !matches!(self.ui.hit_at(at.x, at.y), Some(Target::Subagents(_))) {
                return Err(format!(
                    "the badge {word:?} at ({}, {}) hits {:?}",
                    at.x,
                    at.y,
                    self.ui.hit_at(at.x, at.y)
                ));
            }
            if !(bracketed || opens_button || sidebar_button) {
                continue;
            }
            match self.ui.hit_at(at.x, at.y) {
                None | Some(Target::Popup) | Some(Target::Backdrop) | Some(Target::Terminal) => {
                    return Err(format!(
                        "{word:?} at ({}, {}) is drawn but hits {:?}",
                        at.x,
                        at.y,
                        self.ui.hit_at(at.x, at.y)
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn selection_is_visible(&self) -> Result<(), String> {
        let client = self.ui.client();
        let window = self.ui.window();
        let Some(selected) = client.selected() else {
            return Ok(());
        };
        if self.unfitted || client.popup().is_some() || sidebar_rows(window) == 0 {
            return Ok(());
        }
        let sidebar = layout(window).sidebar;
        let target = Some(Target::Session(selected.clone()));
        if sidebar
            .positions()
            .any(|p| self.ui.hit_at(p.x, p.y) == target)
        {
            Ok(())
        } else {
            Err(format!(
                "the selected session {selected} has no visible row"
            ))
        }
    }
}

/// The leaves and the splits of `pane`, in order.
fn walk(pane: &Pane, leaves: &mut Vec<PaneId>, splits: &mut Vec<PaneId>) {
    match pane {
        Pane::Leaf { id, .. } => leaves.push(*id),
        Pane::Split { id, a, b, .. } => {
            splits.push(*id);
            walk(a, leaves, splits);
            walk(b, leaves, splits);
        }
    }
}

fn run(case: &Case) -> Result<(), TestCaseError> {
    let names: Vec<String> = (1..=case.sessions)
        .map(|i| format!("name-{i:02}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut run = Run {
        ui: Ui::with_sessions(&names, case.size.0, case.size.1),
        made: 0,
        subagents: 0,
        unfitted: false,
        status_moved: false,
    };
    let fail = |run: &Run, after: &str, why: String| {
        TestCaseError::fail(format!("{why}, after {after}:\n{}", run.ui.screen()))
    };
    run.check().map_err(|why| fail(&run, "the start", why))?;
    for (i, step) in case.steps.iter().enumerate() {
        run.apply(step);
        run.check()
            .map_err(|why| fail(&run, &format!("step {i} ({step:?})"), why))?;
    }
    Ok(())
}

#[test]
fn random_ui_steps_never_panic_and_keep_the_invariants() {
    let seed = match Config::default().rng_seed {
        RngSeed::Fixed(seed) => seed,
        RngSeed::Random => SEED,
    };
    let config = Config {
        cases: CASES,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    match runner.run(&case(), |case| run(&case)) {
        Ok(()) => {}
        Err(TestError::Fail(why, case)) => {
            panic!("seed {seed}: {why}\nsteps (shrunk): {case:#?}")
        }
        Err(TestError::Abort(why)) => panic!("seed {seed}: aborted: {why}"),
    }
}

#[test]
fn the_invariants_see_a_selection_scrolled_out_of_view() {
    // The check itself: with the list wheeled away from the selection and the run not told, it fails.
    let names: Vec<String> = (1..=15).map(|i| format!("name-{i:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut run = Run {
        ui: Ui::with_sessions(&names, 80, 12),
        made: 0,
        subagents: 0,
        unfitted: false,
        status_moved: false,
    };
    assert_eq!(run.check(), Ok(()));
    run.ui.scroll("name-15", 20);
    assert_eq!(
        run.ui.client().selected(),
        Some(&SessionId::new("s15")),
        "the wheel leaves the selection"
    );
    assert!(run.check().is_err(), "{}", run.ui.screen());
}
