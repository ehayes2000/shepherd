//! Where the shown page's panes go in a window's view (T49, decision 0022): one pure function, [`arrange`], that both
//! the TUI (to draw and hit-test) and the client (to size each pane's session) use, so they can't disagree.
//!
//! - With more than one page, the view's first row is the page bar.
//! - A page that is one pane is drawn without a border: the default layout looks as shepherd did before layouts.
//! - Otherwise every leaf is a box with a border (its title on the top edge), and each split shares its area by its
//!   ratio: its first half `a` gets the whole cells of [`Ratio`] thousandths of it, `b` the rest.
//! - A bordered pane whose inside is under [`MIN_PANE_COLS`] x [`MIN_PANE_ROWS`] has no terminal
//!   ([`PanePlace::terminal`]): it is drawn as a placeholder and its session is not resized to it.
//! - Where two halves of a split meet, their two border lines are its [`Divider`]: dragging it sets the ratio.
//!
//! The focused pane, input and resizing are the client's ([`super::Client::focused_pane`]).

use crate::layout::{Content, Layout, Pane, PaneId, Ratio, SplitDir};

/// The narrowest inside of a bordered pane that still shows a terminal.
pub const MIN_PANE_COLS: u16 = 8;
/// The shortest inside of a bordered pane that still shows a terminal.
pub const MIN_PANE_ROWS: u16 = 2;

/// A rectangle of cells: `cols` x `rows` from column `x`, row `y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Area {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Area {
    /// Whether cell `col`, `row` is inside.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        (self.x..self.x.saturating_add(self.cols)).contains(&col)
            && (self.y..self.y.saturating_add(self.rows)).contains(&row)
    }

    /// This area less a cell on every side, or nothing if it has no inside.
    fn inside(self) -> Area {
        if self.cols < 2 || self.rows < 2 {
            return Area {
                cols: 0,
                rows: 0,
                ..self
            };
        }
        Area {
            x: self.x + 1,
            y: self.y + 1,
            cols: self.cols - 2,
            rows: self.rows - 2,
        }
    }
}

/// A leaf of the shown page, placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlace {
    pub pane: PaneId,
    /// What it shows, from the layout arranged.
    pub content: Content,
    /// All of it, its border included.
    pub area: Area,
    /// It is drawn in a box: the page has more than one pane.
    pub bordered: bool,
    /// Inside its border (all of it, without one).
    pub inner: Area,
}

impl PanePlace {
    /// Where its session's terminal is drawn, and the size it is resized to: its inside, unless that is too small to
    /// be useful (a bordered pane under [`MIN_PANE_COLS`] x [`MIN_PANE_ROWS`], or no cells at all).
    pub fn terminal(&self) -> Option<Area> {
        let fits = if self.bordered {
            self.inner.cols >= MIN_PANE_COLS && self.inner.rows >= MIN_PANE_ROWS
        } else {
            self.inner.cols > 0 && self.inner.rows > 0
        };
        fits.then_some(self.inner)
    }
}

/// Where the two halves of split `split` meet: their border lines, which drag its ratio.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Divider {
    pub split: PaneId,
    pub dir: SplitDir,
    /// The cells that drag it: the last line of `a` and the first of `b`, across the split.
    pub cells: Area,
    /// The whole split.
    span: Area,
    /// How many cells `a` has along the split.
    a_len: u16,
}

impl Divider {
    /// The split's ratio once the divider is dragged from cell `from` to cell `to` (each `(col, row)`): it moves by
    /// as many cells, stopping short of the split's edges. `None` if that leaves it where it is.
    pub fn dragged(&self, from: (u16, u16), to: (u16, u16)) -> Option<Ratio> {
        let (len, moved) = match self.dir {
            SplitDir::Horizontal => (self.span.cols, i32::from(to.0) - i32::from(from.0)),
            SplitDir::Vertical => (self.span.rows, i32::from(to.1) - i32::from(from.1)),
        };
        if moved == 0 || len < 2 {
            return None;
        }
        let whole = i32::from(len);
        let a = (i32::from(self.a_len) + moved).clamp(1, whole - 1);
        // The smallest ratio that gives `a` exactly `a` cells (see `cells_of`).
        let permille = (a * 1000 + whole - 1) / whole;
        let ratio = Ratio::from_permille(u16::try_from(permille).unwrap_or(Ratio::MAX));
        (cells_of(len, ratio) != self.a_len).then_some(ratio)
    }
}

/// The shown page, placed in a view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Arrangement {
    /// The page bar's row, with more than one page.
    pub page_bar: Option<Area>,
    /// Each leaf of the current page, in order (first halves first).
    pub panes: Vec<PanePlace>,
    /// Each split's divider, outer splits first.
    pub dividers: Vec<Divider>,
}

impl Arrangement {
    pub fn pane(&self, id: PaneId) -> Option<&PanePlace> {
        self.panes.iter().find(|p| p.pane == id)
    }
}

/// `layout`'s current page placed in `view`. See the module docs.
pub fn arrange(layout: &Layout, view: Area) -> Arrangement {
    let mut arranged = Arrangement::default();
    let mut area = view;
    if layout.pages().len() > 1 && view.rows > 0 {
        arranged.page_bar = Some(Area { rows: 1, ..view });
        area = Area {
            y: view.y + 1,
            rows: view.rows - 1,
            ..view
        };
    }
    let root = &layout.current_page().root;
    let bordered = matches!(root, Pane::Split { .. });
    place(root, area, bordered, &mut arranged);
    arranged
}

fn place(pane: &Pane, area: Area, bordered: bool, out: &mut Arrangement) {
    match pane {
        Pane::Leaf { id, content } => out.panes.push(PanePlace {
            pane: *id,
            content: content.clone(),
            area,
            bordered,
            inner: if bordered { area.inside() } else { area },
        }),
        Pane::Split {
            id,
            dir,
            ratio,
            a,
            b,
        } => {
            let (first, second) = split(area, *dir, *ratio);
            let (a_len, cells) = match dir {
                SplitDir::Horizontal => {
                    let x = first.x + first.cols.saturating_sub(1);
                    let cols = u16::from(first.cols > 0) + u16::from(second.cols > 0);
                    (first.cols, Area { x, cols, ..area })
                }
                SplitDir::Vertical => {
                    let y = first.y + first.rows.saturating_sub(1);
                    let rows = u16::from(first.rows > 0) + u16::from(second.rows > 0);
                    (first.rows, Area { y, rows, ..area })
                }
            };
            out.dividers.push(Divider {
                split: *id,
                dir: *dir,
                cells,
                span: area,
                a_len,
            });
            place(a, first, bordered, out);
            place(b, second, bordered, out);
        }
    }
}

/// `area` split in two along `dir`: the first part has `ratio` of it, rounded down to whole cells.
fn split(area: Area, dir: SplitDir, ratio: Ratio) -> (Area, Area) {
    let part = |len| cells_of(len, ratio);
    match dir {
        SplitDir::Horizontal => {
            let a = part(area.cols);
            (
                Area { cols: a, ..area },
                Area {
                    x: area.x + a,
                    cols: area.cols - a,
                    ..area
                },
            )
        }
        SplitDir::Vertical => {
            let a = part(area.rows);
            (
                Area { rows: a, ..area },
                Area {
                    y: area.y + a,
                    rows: area.rows - a,
                    ..area
                },
            )
        }
    }
}

/// How many of `len` cells the first half of a split at `ratio` gets.
fn cells_of(len: u16, ratio: Ratio) -> u16 {
    let a = u32::from(len) * u32::from(ratio.permille()) / 1000;
    u16::try_from(a).unwrap_or(len)
}
