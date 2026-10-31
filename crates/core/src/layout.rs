//! The screen's arrangement: pages of split panes, each showing a session, a view, or nothing (decision 0022).
//!
//! The host owns one [`Layout`], folded from the `_shepherd/ui/layout` frames in its log, and every window draws it at its
//! own size. It changes only through a [`LayoutOp`], applied by [`Layout::apply`], a pure function that returns the new
//! layout or a typed [`LayoutError`]. Which pane is focused is not here: focus is per window (it is where that window's
//! input goes), and [`Layout::neighbor`] says where moving it goes. A session's PTY is sized to the pane showing it on the
//! window that last focused that pane.
//!
//! Ids: pages and panes share one counter, so no id is ever given twice, not even to a page and a pane; a pane keeps its
//! id when it is split (it becomes the split's first half) or when its sibling closes. Ratios are [`Ratio`]s, whole
//! thousandths clamped so that neither half of a split vanishes.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

/// The most pages a layout holds.
pub const MAX_PAGES: usize = 32;
/// The most panes showing something (leaves) one page holds.
pub const MAX_LEAVES: usize = 32;
/// The longest page name, in characters.
pub const MAX_NAME: usize = 64;
/// Ids stay below this, so they are exact in any JSON reader (2^53).
const MAX_ID: u64 = 1 << 53;

macro_rules! layout_id {
    ($t:ident, $what:literal) => {
        #[doc = concat!("A ", $what, "'s id: stable for as long as it exists, and never given to anything else.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $t(u64);

        impl $t {
            pub fn new(id: u64) -> Self {
                Self(id)
            }

            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

layout_id!(PageId, "page");
layout_id!(PaneId, "pane");

/// How much of a split its first half (`a`) takes, in thousandths, between [`Ratio::MIN`] and [`Ratio::MAX`]. Anything
/// outside is clamped, read from the wire included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "u16", into = "u16")]
pub struct Ratio(u16);

impl Ratio {
    pub const MIN: u16 = 50;
    pub const MAX: u16 = 950;

    /// `permille` thousandths, clamped to [`Ratio::MIN`]..=[`Ratio::MAX`].
    pub fn from_permille(permille: u16) -> Self {
        Self(permille.clamp(Self::MIN, Self::MAX))
    }

    pub fn permille(self) -> u16 {
        self.0
    }
}

impl Default for Ratio {
    /// Halves.
    fn default() -> Self {
        Self(500)
    }
}

impl From<u16> for Ratio {
    fn from(permille: u16) -> Self {
        Self::from_permille(permille)
    }
}

impl From<Ratio> for u16 {
    fn from(ratio: Ratio) -> Self {
        ratio.0
    }
}

/// Which way a split divides its space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDir {
    /// Side by side: `a` on the left, `b` on the right.
    Horizontal,
    /// Stacked: `a` above, `b` below.
    Vertical,
}

/// A page of shepherd's own, shown in a pane. Open: a view this build doesn't know is kept as [`ViewKind::Other`] (and
/// drawn as a placeholder).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewKind {
    /// Every session on every machine.
    Status,
    /// This machine and its peers.
    Machines,
    /// The project manager's panel.
    PmPanel,
    /// Worktrees to clean up.
    Cleanup,
    /// The conversation archive.
    Archive,
    #[serde(untagged)]
    Other(String),
}

/// What a pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Content {
    #[default]
    Empty,
    Session(SessionId),
    View(ViewKind),
}

/// A pane: split in two, or showing something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Pane {
    Split {
        id: PaneId,
        dir: SplitDir,
        ratio: Ratio,
        a: Box<Pane>,
        b: Box<Pane>,
    },
    Leaf {
        id: PaneId,
        content: Content,
    },
}

impl Pane {
    pub fn id(&self) -> PaneId {
        match self {
            Self::Split { id, .. } | Self::Leaf { id, .. } => *id,
        }
    }

    /// The pane `id`, this one or one inside it.
    pub fn find(&self, id: PaneId) -> Option<&Pane> {
        if self.id() == id {
            return Some(self);
        }
        match self {
            Self::Split { a, b, .. } => a.find(id).or_else(|| b.find(id)),
            Self::Leaf { .. } => None,
        }
    }

    fn find_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        if self.id() == id {
            return Some(self);
        }
        match self {
            Self::Split { a, b, .. } => match a.find_mut(id) {
                Some(found) => Some(found),
                None => b.find_mut(id),
            },
            Self::Leaf { .. } => None,
        }
    }

    /// How many leaves this pane is.
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Split { a, b, .. } => a.leaf_count() + b.leaf_count(),
            Self::Leaf { .. } => 1,
        }
    }

    /// Every id in this pane, its own first.
    fn ids(&self, out: &mut Vec<u64>) {
        out.push(self.id().get());
        if let Self::Split { a, b, .. } = self {
            a.ids(out);
            b.ids(out);
        }
    }
}

/// A page: a name and a tree of panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub id: PageId,
    pub name: String,
    pub root: Pane,
}

/// The whole arrangement: its pages, in page-bar order, and the one shown. Always sound: at least one page, at most
/// [`MAX_PAGES`], ids unique, the current page one of them. Reading one from the wire or a log checks all of that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLayout")]
pub struct Layout {
    pages: Vec<Page>,
    current: PageId,
    /// The next id to give out: above every id in the layout.
    next_id: u64,
}

/// A layout as read, before it is checked.
#[derive(Deserialize)]
struct RawLayout {
    pages: Vec<Page>,
    current: PageId,
    next_id: u64,
}

/// A layout read from the wire or a log isn't sound, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed layout: {0}")]
pub struct MalformedLayout(String);

impl TryFrom<RawLayout> for Layout {
    type Error = MalformedLayout;

    fn try_from(raw: RawLayout) -> Result<Self, Self::Error> {
        let bad = |why: &str| Err(MalformedLayout(why.to_owned()));
        if raw.pages.is_empty() {
            return bad("no pages");
        }
        if raw.pages.len() > MAX_PAGES {
            return bad("too many pages");
        }
        if !raw.pages.iter().any(|p| p.id == raw.current) {
            return bad("the current page is not one of its pages");
        }
        if raw.next_id >= MAX_ID {
            return bad("ids out of range");
        }
        let mut ids = Vec::new();
        for page in &raw.pages {
            if check_name(&page.name) != Ok(page.name.clone()) {
                return bad("a page name is blank, too long or untrimmed");
            }
            if page.root.leaf_count() > MAX_LEAVES {
                return bad("too many panes on a page");
            }
            ids.push(page.id.get());
            page.root.ids(&mut ids);
        }
        let unique: BTreeSet<u64> = ids.iter().copied().collect();
        if unique.len() != ids.len() {
            return bad("an id is given twice");
        }
        if unique.last().is_some_and(|&max| max >= raw.next_id) {
            return bad("an id is not below the next id");
        }
        Ok(Self {
            pages: raw.pages,
            current: raw.current,
            next_id: raw.next_id,
        })
    }
}

/// A change to the layout, as `_shepherd/ui/apply` carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LayoutOp {
    /// Add a page after the others, with one pane showing `content`, and select it. Unnamed, it is numbered.
    PageNew {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "is_empty")]
        content: Content,
    },
    PageSelect {
        page: PageId,
    },
    PageRename {
        page: PageId,
        name: String,
    },
    /// Close a page and everything on it. The page after it is selected if it was current (the one before, if it was
    /// last); closing the only page leaves a fresh one with one empty pane.
    PageClose {
        page: PageId,
    },
    /// Split `pane` (a leaf or a split, on any page) in two: it keeps its id and becomes the first half, `a`, taking
    /// `ratio` of the space, and a new pane showing `content` is the second, `b`.
    Split {
        pane: PaneId,
        dir: SplitDir,
        #[serde(default)]
        ratio: Ratio,
        #[serde(default, skip_serializing_if = "is_empty")]
        content: Content,
    },
    /// Close `pane` and everything in it: its sibling takes its parent's place. Closing a page's last pane leaves one
    /// empty pane.
    Close {
        pane: PaneId,
    },
    /// Show `content` in leaf `pane`.
    Put {
        pane: PaneId,
        content: Content,
    },
    /// Set split `pane`'s ratio.
    Ratio {
        pane: PaneId,
        ratio: Ratio,
    },
}

fn is_empty(content: &Content) -> bool {
    *content == Content::Empty
}

/// Why an operation was refused. The layout is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("no page {0}")]
    UnknownPage(PageId),
    #[error("no pane {0}")]
    UnknownPane(PaneId),
    #[error("pane {0} is split: only a pane that isn't can show something")]
    NotALeaf(PaneId),
    #[error("pane {0} isn't split")]
    NotASplit(PaneId),
    #[error("no session {0}")]
    UnknownSession(SessionId),
    #[error("a page name can't be blank")]
    EmptyName,
    #[error("a page name can be at most {max} characters")]
    NameTooLong { max: usize },
    #[error("there can be at most {max} pages")]
    TooManyPages { max: usize },
    #[error("a page can show at most {max} panes")]
    TooManyPanes { max: usize },
    #[error("no pane {towards} of pane {pane}")]
    NoPaneThere { pane: PaneId, towards: Towards },
}

/// A direction to move focus in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Towards {
    Left,
    Right,
    Up,
    Down,
}

impl fmt::Display for Towards {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "above",
            Self::Down => "below",
        })
    }
}

/// `name` trimmed, if it may name a page.
fn check_name(name: &str) -> Result<String, LayoutError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LayoutError::EmptyName);
    }
    if name.chars().count() > MAX_NAME {
        return Err(LayoutError::NameTooLong { max: MAX_NAME });
    }
    Ok(name.to_owned())
}

impl Default for Layout {
    /// One page, `1`, with one empty pane.
    fn default() -> Self {
        Self {
            pages: vec![Page {
                id: PageId(1),
                name: "1".to_owned(),
                root: Pane::Leaf {
                    id: PaneId(2),
                    content: Content::Empty,
                },
            }],
            current: PageId(1),
            next_id: 3,
        }
    }
}

impl Layout {
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// The page shown.
    pub fn current(&self) -> PageId {
        self.current
    }

    pub fn current_page(&self) -> &Page {
        self.page(self.current)
            .unwrap_or_else(|| unreachable!("a layout's current page is one of its pages"))
    }

    pub fn page(&self, id: PageId) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    /// Pane `id`, on whichever page it is.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.pages.iter().find_map(|p| p.root.find(id))
    }

    /// The page pane `id` is on.
    pub fn page_of(&self, id: PaneId) -> Option<PageId> {
        self.pages
            .iter()
            .find(|p| p.root.find(id).is_some())
            .map(|p| p.id)
    }

    /// This layout with `op` applied, or why not. A session may be shown only if `listed` says it is.
    pub fn apply(
        &self,
        op: &LayoutOp,
        listed: impl Fn(&SessionId) -> bool,
    ) -> Result<Layout, LayoutError> {
        let shown = |content: &Content| match content {
            Content::Session(id) if !listed(id) => Err(LayoutError::UnknownSession(id.clone())),
            _ => Ok(()),
        };
        let mut next = self.clone();
        match op {
            LayoutOp::PageNew { name, content } => {
                if self.pages.len() >= MAX_PAGES {
                    return Err(LayoutError::TooManyPages { max: MAX_PAGES });
                }
                let name = match name {
                    Some(name) => check_name(name)?,
                    None => (self.pages.len() + 1).to_string(),
                };
                shown(content)?;
                let page = next.fresh_page(name, content.clone());
                next.current = page.id;
                next.pages.push(page);
            }
            LayoutOp::PageSelect { page } => {
                next.page_index(*page)?;
                next.current = *page;
            }
            LayoutOp::PageRename { page, name } => {
                let i = next.page_index(*page)?;
                next.pages[i].name = check_name(name)?;
            }
            LayoutOp::PageClose { page } => {
                let i = next.page_index(*page)?;
                next.pages.remove(i);
                if next.pages.is_empty() {
                    let fresh = next.fresh_page("1".to_owned(), Content::Empty);
                    next.pages.push(fresh);
                }
                if next.current == *page {
                    next.current = next.pages[i.min(next.pages.len() - 1)].id;
                }
            }
            LayoutOp::Split {
                pane,
                dir,
                ratio,
                content,
            } => {
                let i = next.pane_page(*pane)?;
                if next.pages[i].root.leaf_count() >= MAX_LEAVES {
                    return Err(LayoutError::TooManyPanes { max: MAX_LEAVES });
                }
                shown(content)?;
                let split = PaneId(next.fresh_id());
                let leaf = PaneId(next.fresh_id());
                let target = next.pages[i]
                    .root
                    .find_mut(*pane)
                    .ok_or(LayoutError::UnknownPane(*pane))?;
                let old = std::mem::replace(target, placeholder());
                *target = Pane::Split {
                    id: split,
                    dir: *dir,
                    ratio: *ratio,
                    a: Box::new(old),
                    b: Box::new(Pane::Leaf {
                        id: leaf,
                        content: content.clone(),
                    }),
                };
            }
            LayoutOp::Close { pane } => {
                let i = next.pane_page(*pane)?;
                let root = &next.pages[i].root;
                if root.id() == *pane {
                    let id = match root {
                        Pane::Leaf { id, .. } => *id,
                        Pane::Split { .. } => PaneId(next.fresh_id()),
                    };
                    next.pages[i].root = Pane::Leaf {
                        id,
                        content: Content::Empty,
                    };
                } else {
                    close_child(&mut next.pages[i].root, *pane);
                }
            }
            LayoutOp::Put { pane, content } => {
                let i = next.pane_page(*pane)?;
                shown(content)?;
                match next.pages[i].root.find_mut(*pane) {
                    Some(Pane::Leaf { content: shows, .. }) => *shows = content.clone(),
                    Some(Pane::Split { .. }) => return Err(LayoutError::NotALeaf(*pane)),
                    None => return Err(LayoutError::UnknownPane(*pane)),
                }
            }
            LayoutOp::Ratio { pane, ratio } => {
                let i = next.pane_page(*pane)?;
                match next.pages[i].root.find_mut(*pane) {
                    Some(Pane::Split { ratio: r, .. }) => *r = *ratio,
                    Some(Pane::Leaf { .. }) => return Err(LayoutError::NotASplit(*pane)),
                    None => return Err(LayoutError::UnknownPane(*pane)),
                }
            }
        }
        Ok(next)
    }

    /// This layout with every pane showing a session `listed` says isn't listed (closed) showing nothing instead.
    pub fn retain_sessions(&self, listed: impl Fn(&SessionId) -> bool) -> Layout {
        fn retain(pane: &mut Pane, listed: &impl Fn(&SessionId) -> bool) {
            match pane {
                Pane::Leaf { content, .. } => {
                    if matches!(content, Content::Session(id) if !listed(id)) {
                        *content = Content::Empty;
                    }
                }
                Pane::Split { a, b, .. } => {
                    retain(a, listed);
                    retain(b, listed);
                }
            }
        }
        let mut next = self.clone();
        for page in &mut next.pages {
            retain(&mut page.root, &listed);
        }
        next
    }

    /// The leaf focus moves to from leaf `from` towards `towards`: of the leaves on its page that touch its edge on that
    /// side, the one sharing the most of that edge with it, then the topmost or leftmost.
    pub fn neighbor(&self, from: PaneId, towards: Towards) -> Result<PaneId, LayoutError> {
        let page = &self.pages[self.pane_page(from)?];
        if let Some(Pane::Split { .. }) = page.root.find(from) {
            return Err(LayoutError::NotALeaf(from));
        }
        let mut rects = Vec::new();
        page.root.rects(Rect::WHOLE, &mut rects);
        let nowhere = LayoutError::NoPaneThere {
            pane: from,
            towards,
        };
        let Some(&(_, f)) = rects.iter().find(|(id, _)| *id == from) else {
            return Err(nowhere);
        };
        rects
            .iter()
            .filter_map(|&(id, c)| {
                // Whether `c` lies against `f`'s edge on that side, how much of it they share, and where `c` starts
                // along it.
                let (touches, shared, start) = match towards {
                    Towards::Right => (c.x0 == f.x1, overlap(f.y0, f.y1, c.y0, c.y1), c.y0),
                    Towards::Left => (c.x1 == f.x0, overlap(f.y0, f.y1, c.y0, c.y1), c.y0),
                    Towards::Down => (c.y0 == f.y1, overlap(f.x0, f.x1, c.x0, c.x1), c.x0),
                    Towards::Up => (c.y1 == f.y0, overlap(f.x0, f.x1, c.x0, c.x1), c.x0),
                };
                (id != from && touches && shared > 0).then_some((
                    shared,
                    std::cmp::Reverse(start),
                    id,
                ))
            })
            .max()
            .map(|(_, _, id)| id)
            .ok_or(nowhere)
    }

    /// The index of page `id`.
    fn page_index(&self, id: PageId) -> Result<usize, LayoutError> {
        self.pages
            .iter()
            .position(|p| p.id == id)
            .ok_or(LayoutError::UnknownPage(id))
    }

    /// The index of the page pane `id` is on.
    fn pane_page(&self, id: PaneId) -> Result<usize, LayoutError> {
        self.pages
            .iter()
            .position(|p| p.root.find(id).is_some())
            .ok_or(LayoutError::UnknownPane(id))
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// A page named `name` with one pane showing `content`, with fresh ids.
    fn fresh_page(&mut self, name: String, content: Content) -> Page {
        let id = PageId(self.fresh_id());
        let pane = PaneId(self.fresh_id());
        Page {
            id,
            name,
            root: Pane::Leaf { id: pane, content },
        }
    }
}

/// Stands in for a pane while it is moved; never left in a layout.
fn placeholder() -> Pane {
    Pane::Leaf {
        id: PaneId(0),
        content: Content::Empty,
    }
}

/// Closes pane `id`, somewhere below `node` (not `node` itself): its sibling takes its parent's place.
fn close_child(node: &mut Pane, id: PaneId) {
    let Pane::Split { a, b, .. } = node else {
        return;
    };
    let sibling = if a.id() == id {
        std::mem::replace(&mut **b, placeholder())
    } else if b.id() == id {
        std::mem::replace(&mut **a, placeholder())
    } else {
        close_child(a, id);
        close_child(b, id);
        return;
    };
    *node = sibling;
}

/// A leaf's place in an abstract square [`Rect::SIDE`] on a side, the same at every window size: what focus moves by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: u64,
    y0: u64,
    x1: u64,
    y1: u64,
}

impl Rect {
    const SIDE: u64 = 1 << 32;
    const WHOLE: Rect = Rect {
        x0: 0,
        y0: 0,
        x1: Self::SIDE,
        y1: Self::SIDE,
    };
}

/// How much `[a0, a1)` and `[b0, b1)` overlap.
fn overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> u64 {
    a1.min(b1).saturating_sub(a0.max(b0))
}

impl Pane {
    /// Each leaf in this pane, laid out in `area`.
    fn rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Leaf { id, .. } => out.push((*id, area)),
            Self::Split {
                dir, ratio, a, b, ..
            } => {
                let part =
                    |from: u64, to: u64| from + (to - from) * u64::from(ratio.permille()) / 1000;
                let (first, second) = match dir {
                    SplitDir::Horizontal => {
                        let mid = part(area.x0, area.x1);
                        (Rect { x1: mid, ..area }, Rect { x0: mid, ..area })
                    }
                    SplitDir::Vertical => {
                        let mid = part(area.y0, area.y1);
                        (Rect { y1: mid, ..area }, Rect { y0: mid, ..area })
                    }
                };
                a.rects(first, out);
                b.rects(second, out);
            }
        }
    }
}
