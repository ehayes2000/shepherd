//! The host-owned screen layout (decision 0022), through the public `shepherd_core::layout` API.

use std::collections::BTreeSet;

use proptest::prelude::*;
use serde_json::json;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{
    Content, Layout, LayoutError, LayoutOp, MAX_LEAVES, MAX_NAME, MAX_PAGES, Page, PageId, Pane,
    PaneId, Ratio, SplitDir, Towards, ViewKind,
};

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

/// Every session is listed.
fn all_listed(_: &SessionId) -> bool {
    true
}

/// Applies `op` to `layout`, listing `s1` and `s2`, and panics on an error.
fn ok(layout: &Layout, op: LayoutOp) -> Layout {
    layout
        .apply(&op, |s| ["s1", "s2"].contains(&s.as_str()))
        .unwrap_or_else(|e| panic!("{op:?}: {e}"))
}

fn err(layout: &Layout, op: LayoutOp) -> LayoutError {
    match layout.apply(&op, |s| ["s1", "s2"].contains(&s.as_str())) {
        Ok(l) => panic!("{op:?} should fail, got {l:?}"),
        Err(e) => e,
    }
}

/// The only pane of the current page of a layout that has one.
fn root_leaf(layout: &Layout) -> PaneId {
    match &layout.current_page().root {
        Pane::Leaf { id, .. } => *id,
        other => panic!("not a single leaf: {other:?}"),
    }
}

fn split(pane: PaneId, dir: SplitDir) -> LayoutOp {
    LayoutOp::Split {
        pane,
        dir,
        ratio: Ratio::default(),
        content: Content::Empty,
    }
}

fn put(pane: PaneId, content: Content) -> LayoutOp {
    LayoutOp::Put { pane, content }
}

fn new_page(name: Option<&str>) -> LayoutOp {
    LayoutOp::PageNew {
        name: name.map(str::to_owned),
        content: Content::Empty,
    }
}

/// The leaves of `pane`, left to right (top to bottom), with what each shows.
fn leaves(pane: &Pane) -> Vec<(PaneId, Content)> {
    match pane {
        Pane::Leaf { id, content } => vec![(*id, content.clone())],
        Pane::Split { a, b, .. } => {
            let mut out = leaves(a);
            out.extend(leaves(b));
            out
        }
    }
}

fn page_names(layout: &Layout) -> Vec<&str> {
    layout.pages().iter().map(|p| p.name.as_str()).collect()
}

/// Splits the single pane of a fresh layout: `a` (the original) on the left, `b` on the right.
fn two_side_by_side() -> (Layout, PaneId, PaneId) {
    let layout = Layout::default();
    let a = root_leaf(&layout);
    let layout = ok(&layout, split(a, SplitDir::Horizontal));
    let b = leaves(&layout.current_page().root)[1].0;
    (layout, a, b)
}

// ---------- the default ----------

#[test]
fn the_default_layout_is_one_page_with_one_pane_showing_the_windows_selection() {
    let layout = Layout::default();
    assert_eq!(layout.pages().len(), 1);
    assert_eq!(layout.current(), layout.pages()[0].id);
    let page = layout.current_page();
    assert!(matches!(
        page.root,
        Pane::Leaf {
            content: Content::Selected,
            ..
        }
    ));
}

// ---------- the window's selection ----------

#[test]
fn a_selected_pane_can_be_put_split_and_closed_like_any_other() {
    let (layout, a, b) = two_side_by_side();
    // The default pane shows the selection; the new half shows nothing.
    assert_eq!(
        leaves(&layout.current_page().root),
        [(a, Content::Selected), (b, Content::Empty)]
    );
    let layout = ok(&layout, put(b, Content::Selected));
    let layout = ok(&layout, put(a, Content::session(sid("s1"))));
    assert_eq!(
        leaves(&layout.current_page().root),
        [(a, Content::session(sid("s1"))), (b, Content::Selected)]
    );
    let closed = ok(&layout, LayoutOp::Close { pane: a });
    assert_eq!(
        leaves(&closed.current_page().root),
        [(b, Content::Selected)]
    );
    let page = ok(
        &layout,
        LayoutOp::PageNew {
            name: None,
            content: Content::Selected,
        },
    );
    assert_eq!(leaves(&page.current_page().root)[0].1, Content::Selected);
}

#[test]
fn closing_sessions_leaves_selected_panes_alone() {
    let layout = Layout::default();
    assert_eq!(layout.retain_sessions(|_| false), layout);
}

// ---------- peers' sessions ----------

#[test]
fn a_peers_session_can_be_shown_without_this_host_listing_it() {
    let layout = Layout::default();
    let pane = root_leaf(&layout);
    let theirs = Content::peer_session(MachineId::new("m_b"), sid("s9"));
    let shown = layout
        .apply(&put(pane, theirs.clone()), |_| false)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(leaves(&shown.current_page().root), [(pane, theirs)]);
}

#[test]
fn a_peers_session_is_kept_when_this_hosts_sessions_close() {
    let (layout, a, b) = two_side_by_side();
    let theirs = Content::peer_session(MachineId::new("m_b"), sid("s1"));
    let layout = ok(&layout, put(a, theirs.clone()));
    let layout = ok(&layout, put(b, Content::session(sid("s1"))));
    let kept = layout.retain_sessions(|_| false);
    assert_eq!(
        leaves(&kept.current_page().root),
        [(a, theirs), (b, Content::Empty)]
    );
}

#[test]
fn a_malformed_session_is_refused() {
    let layout = Layout::default();
    let pane = root_leaf(&layout);
    for bad in [
        Content::peer_session(MachineId::new("m_b"), sid("No Spaces")),
        Content::peer_session(MachineId::new("m_b"), sid("")),
        Content::peer_session(MachineId::new(""), sid("s1")),
    ] {
        assert!(
            matches!(
                layout.apply(&put(pane, bad.clone()), all_listed),
                Err(LayoutError::InvalidSession(_))
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn content_on_the_wire() {
    let cases = [
        (Content::Empty, json!("empty")),
        (Content::Selected, json!("selected")),
        (
            Content::session(sid("s1")),
            json!({ "session": { "id": "s1" } }),
        ),
        (
            Content::peer_session(MachineId::new("m_b"), sid("s1")),
            json!({ "session": { "machine": "m_b", "id": "s1" } }),
        ),
        (Content::View(ViewKind::Status), json!({ "view": "status" })),
    ];
    for (content, wire) in cases {
        assert_eq!(serde_json::to_value(&content).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Content>(wire).unwrap(), content);
    }
}

// ---------- pages ----------

#[test]
fn a_new_page_has_one_empty_pane_and_is_selected() {
    let layout = ok(&Layout::default(), new_page(Some("work")));
    assert_eq!(layout.pages().len(), 2);
    let page = &layout.pages()[1];
    assert_eq!(page.name, "work");
    assert_eq!(layout.current(), page.id);
    assert!(matches!(
        page.root,
        Pane::Leaf {
            content: Content::Empty,
            ..
        }
    ));
}

#[test]
fn a_new_page_without_a_name_is_numbered() {
    let layout = ok(&Layout::default(), new_page(None));
    assert_eq!(page_names(&layout), ["1", "2"]);
}

#[test]
fn a_new_page_can_open_on_a_session_or_a_view() {
    let layout = ok(
        &Layout::default(),
        LayoutOp::PageNew {
            name: Some("status".into()),
            content: Content::View(ViewKind::Status),
        },
    );
    assert_eq!(
        leaves(&layout.current_page().root)[0].1,
        Content::View(ViewKind::Status)
    );
    let layout = ok(
        &layout,
        LayoutOp::PageNew {
            name: None,
            content: Content::session(sid("s1")),
        },
    );
    assert_eq!(
        leaves(&layout.current_page().root)[0].1,
        Content::session(sid("s1"))
    );
}

#[test]
fn a_new_page_on_a_session_that_is_not_listed_is_refused() {
    let op = LayoutOp::PageNew {
        name: None,
        content: Content::session(sid("gone")),
    };
    assert_eq!(
        err(&Layout::default(), op),
        LayoutError::UnknownSession(sid("gone"))
    );
}

#[test]
fn page_names_are_trimmed_and_must_not_be_blank_or_too_long() {
    let layout = ok(&Layout::default(), new_page(Some("  work \t")));
    assert_eq!(page_names(&layout), ["1", "work"]);
    assert_eq!(
        err(&Layout::default(), new_page(Some(" \n "))),
        LayoutError::EmptyName
    );
    let long = "é".repeat(MAX_NAME + 1);
    assert_eq!(
        err(&Layout::default(), new_page(Some(&long))),
        LayoutError::NameTooLong { max: MAX_NAME }
    );
    let longest = "é".repeat(MAX_NAME);
    ok(&Layout::default(), new_page(Some(&longest)));
}

#[test]
fn there_can_be_at_most_max_pages() {
    let mut layout = Layout::default();
    for _ in 1..MAX_PAGES {
        layout = ok(&layout, new_page(None));
    }
    assert_eq!(layout.pages().len(), MAX_PAGES);
    assert_eq!(
        err(&layout, new_page(None)),
        LayoutError::TooManyPages { max: MAX_PAGES }
    );
}

#[test]
fn selecting_a_page_makes_it_current() {
    let first = Layout::default().current();
    let layout = ok(&Layout::default(), new_page(None));
    let layout = ok(&layout, LayoutOp::PageSelect { page: first });
    assert_eq!(layout.current(), first);
}

#[test]
fn renaming_a_page_changes_only_its_name() {
    let layout = ok(&Layout::default(), new_page(Some("a")));
    let page = layout.current();
    let renamed = ok(
        &layout,
        LayoutOp::PageRename {
            page,
            name: " b ".into(),
        },
    );
    assert_eq!(page_names(&renamed), ["1", "b"]);
    assert_eq!(renamed.current(), page);
    assert_eq!(renamed.pages()[1].root, layout.pages()[1].root);
}

#[test]
fn renaming_a_page_checks_the_name() {
    let layout = Layout::default();
    let page = layout.current();
    let rename = |name: &str| LayoutOp::PageRename {
        page,
        name: name.into(),
    };
    assert_eq!(err(&layout, rename("  ")), LayoutError::EmptyName);
    assert_eq!(
        err(&layout, rename(&"x".repeat(MAX_NAME + 1))),
        LayoutError::NameTooLong { max: MAX_NAME }
    );
}

#[test]
fn closing_the_current_page_selects_the_next_one_or_the_last() {
    let layout = ok(&Layout::default(), new_page(Some("b")));
    let layout = ok(&layout, new_page(Some("c")));
    let [a, b, c] = [0, 1, 2].map(|i| layout.pages()[i].id);
    let middle = ok(
        &ok(&layout, LayoutOp::PageSelect { page: b }),
        LayoutOp::PageClose { page: b },
    );
    assert_eq!(page_names(&middle), ["1", "c"]);
    assert_eq!(middle.current(), c);
    let last = ok(&layout, LayoutOp::PageClose { page: c });
    assert_eq!(page_names(&last), ["1", "b"]);
    assert_eq!(last.current(), b);
    let other = ok(&layout, LayoutOp::PageClose { page: a });
    assert_eq!(other.current(), c);
}

#[test]
fn closing_the_last_page_leaves_one_empty_page() {
    let layout = Layout::default();
    let page = layout.current();
    let pane = root_leaf(&layout);
    let layout = ok(&layout, put(pane, Content::session(sid("s1"))));
    let closed = ok(&layout, LayoutOp::PageClose { page });
    assert_eq!(closed.pages().len(), 1);
    assert_ne!(closed.current(), page, "a fresh page, with a fresh id");
    assert_eq!(
        leaves(&closed.current_page().root),
        [(root_leaf(&closed), Content::Empty)]
    );
    assert_ne!(root_leaf(&closed), pane);
}

#[test]
fn page_operations_on_an_unknown_page_are_refused() {
    let layout = Layout::default();
    let nowhere = PageId::new(999);
    for op in [
        LayoutOp::PageSelect { page: nowhere },
        LayoutOp::PageRename {
            page: nowhere,
            name: "x".into(),
        },
        LayoutOp::PageClose { page: nowhere },
    ] {
        assert_eq!(err(&layout, op), LayoutError::UnknownPage(nowhere));
    }
    // A pane's id is not a page's.
    let pane = root_leaf(&layout);
    let as_page = PageId::new(pane.get());
    assert_eq!(
        err(&layout, LayoutOp::PageSelect { page: as_page }),
        LayoutError::UnknownPage(as_page)
    );
}

// ---------- splitting ----------

#[test]
fn splitting_a_pane_keeps_it_as_the_first_half_and_adds_a_new_one() {
    let layout = Layout::default();
    let a = root_leaf(&layout);
    let layout = ok(&layout, put(a, Content::session(sid("s1"))));
    let layout = ok(
        &layout,
        LayoutOp::Split {
            pane: a,
            dir: SplitDir::Vertical,
            ratio: Ratio::from_permille(300),
            content: Content::session(sid("s2")),
        },
    );
    let Pane::Split {
        id,
        dir,
        ratio,
        a: first,
        b: second,
    } = &layout.current_page().root
    else {
        panic!("not split: {layout:?}");
    };
    assert_eq!(*dir, SplitDir::Vertical);
    assert_eq!(ratio.permille(), 300);
    assert_eq!(
        **first,
        Pane::Leaf {
            id: a,
            content: Content::session(sid("s1"))
        }
    );
    let Pane::Leaf { id: b, content } = &**second else {
        panic!("{second:?}");
    };
    assert_eq!(*content, Content::session(sid("s2")));
    assert!(*id != a && *b != a && id != b);
}

#[test]
fn splitting_a_split_pane_splits_the_whole_of_it() {
    let (layout, a, b) = two_side_by_side();
    let top = layout.current_page().root.id();
    let layout = ok(&layout, split(top, SplitDir::Vertical));
    let Pane::Split { a: first, dir, .. } = &layout.current_page().root else {
        panic!("{layout:?}");
    };
    assert_eq!(*dir, SplitDir::Vertical);
    assert_eq!(first.id(), top);
    let ids: Vec<PaneId> = leaves(&layout.current_page().root)
        .iter()
        .map(|l| l.0)
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(&ids[..2], [a, b]);
}

#[test]
fn splitting_refuses_an_unknown_pane_or_session() {
    let layout = Layout::default();
    let nowhere = PaneId::new(999);
    assert_eq!(
        err(&layout, split(nowhere, SplitDir::Horizontal)),
        LayoutError::UnknownPane(nowhere)
    );
    let op = LayoutOp::Split {
        pane: root_leaf(&layout),
        dir: SplitDir::Horizontal,
        ratio: Ratio::default(),
        content: Content::session(sid("gone")),
    };
    assert_eq!(err(&layout, op), LayoutError::UnknownSession(sid("gone")));
}

#[test]
fn a_page_holds_at_most_max_leaves() {
    let mut layout = Layout::default();
    let pane = root_leaf(&layout);
    for _ in 1..MAX_LEAVES {
        layout = ok(&layout, split(pane, SplitDir::Horizontal));
    }
    assert_eq!(leaves(&layout.current_page().root).len(), MAX_LEAVES);
    assert_eq!(
        err(&layout, split(pane, SplitDir::Vertical)),
        LayoutError::TooManyPanes { max: MAX_LEAVES }
    );
    // Another page has room of its own.
    let layout = ok(&layout, new_page(None));
    ok(&layout, split(root_leaf(&layout), SplitDir::Vertical));
}

#[test]
fn panes_on_other_pages_can_be_split_without_selecting_them() {
    let layout = Layout::default();
    let first = root_leaf(&layout);
    let layout = ok(&layout, new_page(None));
    let split = ok(&layout, split(first, SplitDir::Horizontal));
    assert_eq!(split.current(), layout.current());
    assert_eq!(leaves(&split.pages()[0].root).len(), 2);
}

// ---------- closing panes ----------

#[test]
fn closing_a_pane_puts_its_sibling_in_its_parents_place() {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, put(b, Content::session(sid("s2"))));
    let closed = ok(&layout, LayoutOp::Close { pane: a });
    assert_eq!(
        closed.current_page().root,
        Pane::Leaf {
            id: b,
            content: Content::session(sid("s2"))
        }
    );
}

#[test]
fn closing_a_nested_pane_keeps_the_rest_of_the_tree() {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, split(b, SplitDir::Vertical));
    let c = leaves(&layout.current_page().root)[2].0;
    let closed = ok(&layout, LayoutOp::Close { pane: b });
    let Pane::Split {
        a: left, b: right, ..
    } = &closed.current_page().root
    else {
        panic!("{closed:?}");
    };
    assert_eq!(left.id(), a);
    assert_eq!(right.id(), c);
}

#[test]
fn closing_a_split_closes_everything_in_it() {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, split(b, SplitDir::Vertical));
    let Pane::Split { b: right, .. } = &layout.current_page().root else {
        panic!("{layout:?}");
    };
    let closed = ok(&layout, LayoutOp::Close { pane: right.id() });
    assert_eq!(
        leaves(&closed.current_page().root),
        [(a, Content::Selected)]
    );
}

#[test]
fn closing_the_last_pane_leaves_one_empty_pane() {
    let layout = Layout::default();
    let pane = root_leaf(&layout);
    let layout = ok(&layout, put(pane, Content::session(sid("s1"))));
    let closed = ok(&layout, LayoutOp::Close { pane });
    assert_eq!(
        leaves(&closed.current_page().root),
        [(pane, Content::Empty)]
    );

    let (layout, _, _) = two_side_by_side();
    let root = layout.current_page().root.id();
    let closed = ok(&layout, LayoutOp::Close { pane: root });
    let only = leaves(&closed.current_page().root);
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].1, Content::Empty);
}

#[test]
fn closing_an_unknown_pane_is_refused() {
    let nowhere = PaneId::new(999);
    assert_eq!(
        err(&Layout::default(), LayoutOp::Close { pane: nowhere }),
        LayoutError::UnknownPane(nowhere)
    );
}

#[test]
fn ids_are_never_reused_after_a_close() {
    let (layout, _, b) = two_side_by_side();
    let closed = ok(&layout, LayoutOp::Close { pane: b });
    let again = ok(&closed, split(root_leaf(&closed), SplitDir::Horizontal));
    let ids: Vec<PaneId> = leaves(&again.current_page().root)
        .iter()
        .map(|l| l.0)
        .collect();
    assert!(!ids.contains(&b), "{ids:?} reuses {b}");
    assert_ne!(
        again.current_page().root.id(),
        layout.current_page().root.id()
    );
}

// ---------- what a pane shows ----------

#[test]
fn putting_a_session_or_a_view_in_a_pane() {
    let layout = Layout::default();
    let pane = root_leaf(&layout);
    let layout = ok(&layout, put(pane, Content::session(sid("s1"))));
    assert_eq!(
        leaves(&layout.current_page().root),
        [(pane, Content::session(sid("s1")))]
    );
    let layout = ok(&layout, put(pane, Content::View(ViewKind::Machines)));
    assert_eq!(
        leaves(&layout.current_page().root),
        [(pane, Content::View(ViewKind::Machines))]
    );
    let layout = ok(&layout, put(pane, Content::Empty));
    assert_eq!(
        leaves(&layout.current_page().root),
        [(pane, Content::Empty)]
    );
}

#[test]
fn one_session_can_show_in_several_panes() {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, put(a, Content::session(sid("s1"))));
    let layout = ok(&layout, put(b, Content::session(sid("s1"))));
    assert_eq!(
        leaves(&layout.current_page().root),
        [
            (a, Content::session(sid("s1"))),
            (b, Content::session(sid("s1")))
        ]
    );
}

#[test]
fn putting_refuses_a_split_an_unknown_pane_or_an_unlisted_session() {
    let (layout, a, _) = two_side_by_side();
    let root = layout.current_page().root.id();
    assert_eq!(
        err(&layout, put(root, Content::Empty)),
        LayoutError::NotALeaf(root)
    );
    let nowhere = PaneId::new(999);
    assert_eq!(
        err(&layout, put(nowhere, Content::Empty)),
        LayoutError::UnknownPane(nowhere)
    );
    assert_eq!(
        err(&layout, put(a, Content::session(sid("gone")))),
        LayoutError::UnknownSession(sid("gone"))
    );
}

#[test]
fn views_this_build_does_not_know_are_kept_as_named() {
    let view: ViewKind = serde_json::from_value(json!("future-thing")).unwrap();
    assert_eq!(view, ViewKind::Other("future-thing".into()));
    assert_eq!(serde_json::to_value(&view).unwrap(), json!("future-thing"));
    assert_eq!(
        serde_json::from_value::<ViewKind>(json!("status")).unwrap(),
        ViewKind::Status
    );
}

// ---------- ratios ----------

#[test]
fn ratios_are_clamped_so_neither_half_vanishes() {
    assert_eq!(Ratio::from_permille(0).permille(), Ratio::MIN);
    assert_eq!(Ratio::from_permille(1000).permille(), Ratio::MAX);
    assert_eq!(Ratio::from_permille(u16::MAX).permille(), Ratio::MAX);
    assert_eq!(Ratio::from_permille(420).permille(), 420);
    assert_eq!(Ratio::default().permille(), 500);
    // Read from the wire through the same clamp.
    let read: Ratio = serde_json::from_value(json!(5)).unwrap();
    assert_eq!(read.permille(), Ratio::MIN);
    assert!(serde_json::from_value::<Ratio>(json!(0.5)).is_err());
}

#[test]
fn setting_a_ratio_changes_only_that_split() {
    let (layout, a, b) = two_side_by_side();
    let root = layout.current_page().root.id();
    let set = ok(
        &layout,
        LayoutOp::Ratio {
            pane: root,
            ratio: Ratio::from_permille(2),
        },
    );
    let Pane::Split {
        ratio,
        a: first,
        b: second,
        ..
    } = &set.current_page().root
    else {
        panic!("{set:?}");
    };
    assert_eq!(ratio.permille(), Ratio::MIN);
    assert_eq!((first.id(), second.id()), (a, b));
}

#[test]
fn setting_a_ratio_refuses_a_leaf_or_an_unknown_pane() {
    let (layout, a, _) = two_side_by_side();
    let op = |pane| LayoutOp::Ratio {
        pane,
        ratio: Ratio::default(),
    };
    assert_eq!(err(&layout, op(a)), LayoutError::NotASplit(a));
    let nowhere = PaneId::new(999);
    assert_eq!(err(&layout, op(nowhere)), LayoutError::UnknownPane(nowhere));
}

// ---------- closed sessions ----------

#[test]
fn panes_showing_a_session_no_longer_listed_become_empty_on_every_page() {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, put(a, Content::session(sid("s1"))));
    let layout = ok(&layout, put(b, Content::session(sid("s2"))));
    let layout = ok(
        &layout,
        LayoutOp::PageNew {
            name: None,
            content: Content::session(sid("s1")),
        },
    );
    let kept = layout.retain_sessions(|s| s.as_str() == "s2");
    assert_eq!(
        leaves(&kept.pages()[0].root),
        [(a, Content::Empty), (b, Content::session(sid("s2")))]
    );
    assert_eq!(leaves(&kept.pages()[1].root)[0].1, Content::Empty);
    assert_eq!(kept.current(), layout.current());
}

// ---------- moving focus ----------

/// ```text
/// +-----+-----+
/// |     |  b  |
/// |  a  +--+--+
/// |     |c | d|
/// +-----+--+--+
/// ```
fn grid() -> (Layout, [PaneId; 4]) {
    let (layout, a, b) = two_side_by_side();
    let layout = ok(&layout, split(b, SplitDir::Vertical));
    let c = leaves(&layout.current_page().root)[2].0;
    let layout = ok(&layout, split(c, SplitDir::Horizontal));
    let d = leaves(&layout.current_page().root)[3].0;
    (layout, [a, b, c, d])
}

#[test]
fn focus_moves_to_the_adjacent_pane_in_each_direction() {
    let (layout, [a, b, c, d]) = grid();
    assert_eq!(
        layout.neighbor(a, Towards::Right),
        Ok(b),
        "b overlaps a more than c does"
    );
    assert_eq!(layout.neighbor(b, Towards::Left), Ok(a));
    assert_eq!(
        layout.neighbor(b, Towards::Down),
        Ok(c),
        "c is the leftmost of the two below b"
    );
    assert_eq!(layout.neighbor(d, Towards::Up), Ok(b));
    assert_eq!(layout.neighbor(c, Towards::Right), Ok(d));
    assert_eq!(layout.neighbor(d, Towards::Left), Ok(c));
    assert_eq!(layout.neighbor(c, Towards::Left), Ok(a));
}

#[test]
fn focus_has_nowhere_to_go_at_an_edge() {
    let (layout, [a, b, _, d]) = grid();
    for (pane, towards) in [
        (a, Towards::Left),
        (a, Towards::Up),
        (a, Towards::Down),
        (b, Towards::Up),
        (b, Towards::Right),
        (d, Towards::Down),
    ] {
        assert_eq!(
            layout.neighbor(pane, towards),
            Err(LayoutError::NoPaneThere { pane, towards })
        );
    }
}

#[test]
fn focus_stays_on_the_page_and_refuses_unknown_or_split_panes() {
    let (layout, [a, ..]) = grid();
    let layout = ok(&layout, new_page(None));
    assert_eq!(layout.neighbor(a, Towards::Right).map(|_| ()), Ok(()));
    let other = root_leaf(&layout);
    assert_eq!(
        layout.neighbor(other, Towards::Left),
        Err(LayoutError::NoPaneThere {
            pane: other,
            towards: Towards::Left
        })
    );
    let nowhere = PaneId::new(999);
    assert_eq!(
        layout.neighbor(nowhere, Towards::Left),
        Err(LayoutError::UnknownPane(nowhere))
    );
    let root = layout.pages()[0].root.id();
    assert_eq!(
        layout.neighbor(root, Towards::Left),
        Err(LayoutError::NotALeaf(root))
    );
}

#[test]
fn panes_are_found_by_id_on_any_page() {
    let (layout, [a, ..]) = grid();
    let layout = ok(&layout, new_page(None));
    assert_eq!(layout.page_of(a), Some(layout.pages()[0].id));
    assert_eq!(
        layout.pane(a),
        Some(&Pane::Leaf {
            id: a,
            content: Content::Selected
        })
    );
    assert_eq!(layout.pane(PaneId::new(999)), None);
    assert_eq!(
        layout.page(layout.current()).map(|p| p.id),
        Some(layout.current())
    );
}

// ---------- on the wire and in the log ----------

#[test]
fn a_layout_reads_back_as_written() {
    let (layout, [a, b, c, _]) = grid();
    let layout = ok(&layout, put(a, Content::session(sid("s1"))));
    let layout = ok(
        &layout,
        put(b, Content::peer_session(MachineId::new("m_b"), sid("s2"))),
    );
    let layout = ok(&layout, put(c, Content::Selected));
    let layout = ok(&layout, new_page(Some("views")));
    let layout = ok(
        &layout,
        put(
            root_leaf(&layout),
            Content::View(ViewKind::Other("x".into())),
        ),
    );
    let value = serde_json::to_value(&layout).unwrap();
    assert_eq!(serde_json::from_value::<Layout>(value).unwrap(), layout);
}

#[test]
fn the_default_layout_on_the_wire() {
    assert_eq!(
        serde_json::to_value(Layout::default()).unwrap(),
        json!({
            "pages": [{ "id": 1, "name": "1", "root": { "kind": "leaf", "id": 2, "content": "selected" } }],
            "current": 1,
            "next_id": 3,
        })
    );
}

#[test]
fn ops_on_the_wire() {
    let op: LayoutOp =
        serde_json::from_value(json!({ "kind": "split", "pane": 2, "dir": "horizontal" })).unwrap();
    assert_eq!(op, split(PaneId::new(2), SplitDir::Horizontal));
    let op: LayoutOp = serde_json::from_value(
        json!({ "kind": "put", "pane": 2, "content": { "session": { "id": "s1" } } }),
    )
    .unwrap();
    assert_eq!(op, put(PaneId::new(2), Content::session(sid("s1"))));
    let op: LayoutOp =
        serde_json::from_value(json!({ "kind": "ratio", "pane": 3, "ratio": 2000 })).unwrap();
    assert_eq!(
        op,
        LayoutOp::Ratio {
            pane: PaneId::new(3),
            ratio: Ratio::from_permille(Ratio::MAX)
        }
    );
    let op: LayoutOp = serde_json::from_value(json!({ "kind": "page-new" })).unwrap();
    assert_eq!(op, new_page(None));
    for op in [
        new_page(Some("x")),
        LayoutOp::PageSelect {
            page: PageId::new(1),
        },
        LayoutOp::PageRename {
            page: PageId::new(1),
            name: "y".into(),
        },
        LayoutOp::PageClose {
            page: PageId::new(1),
        },
        LayoutOp::Close {
            pane: PaneId::new(2),
        },
        put(PaneId::new(2), Content::View(ViewKind::PmPanel)),
    ] {
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(serde_json::from_value::<LayoutOp>(value).unwrap(), op);
    }
}

#[test]
fn a_malformed_layout_does_not_read() {
    let leaf = |id: u64| json!({ "kind": "leaf", "id": id, "content": "empty" });
    let page = |id: u64, root| json!({ "id": id, "name": "p", "root": root });
    let bad = [
        // No pages.
        json!({ "pages": [], "current": 1, "next_id": 3 }),
        // Current is not a page.
        json!({ "pages": [page(1, leaf(2))], "current": 2, "next_id": 3 }),
        // A pane and a page share an id.
        json!({ "pages": [page(1, leaf(1))], "current": 1, "next_id": 3 }),
        // Two panes share an id.
        json!({ "pages": [page(1, json!({ "kind": "split", "id": 2, "dir": "vertical", "ratio": 500, "a": leaf(3), "b": leaf(3) }))], "current": 1, "next_id": 4 }),
        // An id the next one would reuse.
        json!({ "pages": [page(1, leaf(2))], "current": 1, "next_id": 2 }),
        // A blank name, an untrimmed one, a long one.
        json!({ "pages": [{ "id": 1, "name": " ", "root": leaf(2) }], "current": 1, "next_id": 3 }),
        json!({ "pages": [{ "id": 1, "name": " p", "root": leaf(2) }], "current": 1, "next_id": 3 }),
        json!({ "pages": [{ "id": 1, "name": "p".repeat(MAX_NAME + 1), "root": leaf(2) }], "current": 1, "next_id": 3 }),
        // A session that can't be one, this host's or a peer's, or a peer with no name.
        json!({ "pages": [page(1, json!({ "kind": "leaf", "id": 2, "content": { "session": { "id": "A B" } } }))], "current": 1, "next_id": 3 }),
        json!({ "pages": [page(1, json!({ "kind": "leaf", "id": 2, "content": { "session": { "machine": "m_b", "id": "" } } }))], "current": 1, "next_id": 3 }),
        json!({ "pages": [page(1, json!({ "kind": "leaf", "id": 2, "content": { "session": { "machine": "", "id": "s1" } } }))], "current": 1, "next_id": 3 }),
        // Ids past what JSON readers hold exactly.
        json!({ "pages": [page(1, leaf(2))], "current": 1, "next_id": 1u64 << 53 }),
    ];
    for value in bad {
        assert!(
            serde_json::from_value::<Layout>(value.clone()).is_err(),
            "{value}"
        );
    }
    let too_many_pages: Vec<_> = (0..=MAX_PAGES as u64)
        .map(|i| page(2 * i + 1, leaf(2 * i + 2)))
        .collect();
    let value = json!({ "pages": too_many_pages, "current": 1, "next_id": 1000 });
    assert!(serde_json::from_value::<Layout>(value).is_err());

    // A page as full as it can be reads; one more pane does not.
    let mut full = Layout::default();
    let pane = match full.current_page().root {
        Pane::Leaf { id, .. } => id,
        Pane::Split { .. } => unreachable!(),
    };
    for _ in 1..MAX_LEAVES {
        full = ok(&full, split(pane, SplitDir::Horizontal));
    }
    let mut value = serde_json::to_value(&full).unwrap();
    assert_eq!(
        serde_json::from_value::<Layout>(value.clone()).unwrap(),
        full
    );
    let root = value["pages"][0]["root"].take();
    value["pages"][0]["root"] = json!({ "kind": "split", "id": 900, "dir": "vertical", "ratio": 500, "a": root, "b": leaf(901) });
    value["next_id"] = json!(902);
    assert!(serde_json::from_value::<Layout>(value).is_err());
}

// ---------- invariants ----------

#[derive(Debug, Clone)]
enum Step {
    /// The op, with pane and page ids picked by index from the layout as it is then.
    PageNew(Option<u8>, u8),
    PageSelect(u8),
    PageRename(u8, u8),
    PageClose(u8),
    Split(u8, bool, u16, u8),
    Close(u8),
    Put(u8, u8),
    Ratio(u8, u16),
    /// A session stops being listed.
    SessionGone(u8),
    /// A made-up id.
    Bogus(u64),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (proptest::option::of(0u8..4), 0u8..10).prop_map(|(n, c)| Step::PageNew(n, c)),
        any::<u8>().prop_map(Step::PageSelect),
        (any::<u8>(), 0u8..4).prop_map(|(p, n)| Step::PageRename(p, n)),
        any::<u8>().prop_map(Step::PageClose),
        (any::<u8>(), any::<bool>(), any::<u16>(), 0u8..10)
            .prop_map(|(p, d, r, c)| Step::Split(p, d, r, c)),
        any::<u8>().prop_map(Step::Close),
        (any::<u8>(), 0u8..10).prop_map(|(p, c)| Step::Put(p, c)),
        (any::<u8>(), any::<u16>()).prop_map(|(p, r)| Step::Ratio(p, r)),
        (0u8..4).prop_map(Step::SessionGone),
        (0u64..64).prop_map(Step::Bogus),
    ]
}

fn peer() -> MachineId {
    MachineId::new("m_peer")
}

/// Content index `c`: this host's sessions s0..s3 (listed or not), views, the window's selection, a peer's session, a
/// malformed one, or empty.
fn content(c: u8) -> Content {
    match c {
        0..4 => Content::session(sid(&format!("s{c}"))),
        4 => Content::View(ViewKind::Status),
        5 => Content::View(ViewKind::Other("x".into())),
        6 => Content::Selected,
        7 => Content::peer_session(peer(), sid("p1")),
        8 => Content::peer_session(peer(), sid("Not A Session")),
        _ => Content::Empty,
    }
}

fn all_panes(pane: &Pane, out: &mut Vec<PaneId>) {
    out.push(pane.id());
    if let Pane::Split { a, b, .. } = pane {
        all_panes(a, out);
        all_panes(b, out);
    }
}

fn check_invariants(layout: &Layout, listed: &BTreeSet<SessionId>) -> Result<(), TestCaseError> {
    prop_assert!(!layout.pages().is_empty());
    prop_assert!(layout.pages().len() <= MAX_PAGES);
    prop_assert!(layout.page(layout.current()).is_some());
    let mut ids: Vec<u64> = Vec::new();
    for page in layout.pages() {
        ids.push(page.id.get());
        let mut panes = Vec::new();
        all_panes(&page.root, &mut panes);
        ids.extend(panes.iter().map(|p| p.get()));
        let page_leaves = leaves(&page.root);
        prop_assert!(page_leaves.len() <= MAX_LEAVES);
        for (_, content) in page_leaves {
            match content {
                Content::Session { machine: None, id } => {
                    prop_assert!(listed.contains(&id), "{id} is not listed");
                }
                // A peer's session is the peer's to list: it stays whatever this host lists.
                Content::Session {
                    machine: Some(m), ..
                } => prop_assert_eq!(m, peer()),
                _ => {}
            }
        }
        check_ratios(&page.root)?;
    }
    let unique: BTreeSet<u64> = ids.iter().copied().collect();
    prop_assert_eq!(unique.len(), ids.len(), "ids repeat: {:?}", ids);
    Ok(())
}

fn check_ratios(pane: &Pane) -> Result<(), TestCaseError> {
    if let Pane::Split { ratio, a, b, .. } = pane {
        prop_assert!((Ratio::MIN..=Ratio::MAX).contains(&ratio.permille()));
        check_ratios(a)?;
        check_ratios(b)?;
    }
    Ok(())
}

/// The pane at index `i` of every pane on every page, splits included.
fn pick_pane(layout: &Layout, i: u8) -> PaneId {
    let mut panes = Vec::new();
    for page in layout.pages() {
        all_panes(&page.root, &mut panes);
    }
    panes[usize::from(i) % panes.len()]
}

fn pick_page(layout: &Layout, i: u8) -> PageId {
    layout.pages()[usize::from(i) % layout.pages().len()].id
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn any_sequence_of_operations_keeps_the_layout_sound(steps in proptest::collection::vec(step(), 0..80)) {
        let mut listed: BTreeSet<SessionId> = (0..4).map(|i| sid(&format!("s{i}"))).collect();
        let mut layout = Layout::default();
        for step in steps {
            let op = match step {
                Step::PageNew(name, c) => LayoutOp::PageNew { name: name.map(|n| format!("p{n}")), content: content(c) },
                Step::PageSelect(p) => LayoutOp::PageSelect { page: pick_page(&layout, p) },
                Step::PageRename(p, n) => LayoutOp::PageRename { page: pick_page(&layout, p), name: format!("r{n}") },
                Step::PageClose(p) => LayoutOp::PageClose { page: pick_page(&layout, p) },
                Step::Split(p, d, r, c) => LayoutOp::Split {
                    pane: pick_pane(&layout, p),
                    dir: if d { SplitDir::Horizontal } else { SplitDir::Vertical },
                    ratio: Ratio::from_permille(r),
                    content: content(c),
                },
                Step::Close(p) => LayoutOp::Close { pane: pick_pane(&layout, p) },
                Step::Put(p, c) => LayoutOp::Put { pane: pick_pane(&layout, p), content: content(c) },
                Step::Ratio(p, r) => LayoutOp::Ratio { pane: pick_pane(&layout, p), ratio: Ratio::from_permille(r) },
                Step::SessionGone(s) => {
                    listed.remove(&sid(&format!("s{s}")));
                    layout = layout.retain_sessions(|s| listed.contains(s));
                    check_invariants(&layout, &listed)?;
                    continue;
                }
                Step::Bogus(id) => LayoutOp::Close { pane: PaneId::new(id) },
            };
            // A refused op is one of the typed errors, never a panic, and changes nothing.
            if let Ok(next) = layout.apply(&op, |s| listed.contains(s)) {
                layout = next;
            }
            check_invariants(&layout, &listed)?;
            let value = serde_json::to_value(&layout).unwrap();
            prop_assert_eq!(&serde_json::from_value::<Layout>(value).unwrap(), &layout);
        }
        // Every leaf can look for a neighbour without panicking.
        for page in layout.pages() {
            for (leaf, _) in leaves(&page.root) {
                for towards in [Towards::Left, Towards::Right, Towards::Up, Towards::Down] {
                    let _ = layout.neighbor(leaf, towards);
                }
            }
        }
    }
}

#[test]
fn whether_a_session_may_be_shown_is_the_callers_to_say() {
    let layout = Layout::default();
    let op = put(root_leaf(&layout), Content::session(sid("anything")));
    assert!(layout.apply(&op, all_listed).is_ok());
    assert_eq!(
        layout.apply(&op, |_| false),
        Err(LayoutError::UnknownSession(sid("anything")))
    );
    let page: &Page = layout.current_page();
    assert_eq!(page.id, layout.current());
}
