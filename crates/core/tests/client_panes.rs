//! A window's view of its host's layout (T49): where each pane goes ([`arrange`]), which pane is focused (per window),
//! where input goes, what each pane's session is resized to, the pane menu and the layout ops it asks the host for.

use shepherd_core::client::panes::{Area, MIN_PANE_COLS, MIN_PANE_ROWS, arrange};
use shepherd_core::client::{Cell, ClientCommand, Intent, PaneMenuItem, Popup};
use shepherd_core::ids::SessionId;
use shepherd_core::layout::{Content, Layout, LayoutOp, Pane, PaneId, Ratio, SplitDir, ViewKind};
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{
    ClientNotification, ClientRequest, InputKind, SessionInput, SessionResize, UiApply,
};
use shepherd_testkit::builders::{ClientDriver, notifications, requests};

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn input(session: &str, data: &[u8]) -> ClientNotification {
    ClientNotification::SessionInput(SessionInput {
        session: sid(session),
        data: data.to_vec(),
        kind: InputKind::Keys,
    })
}

fn resize(session: &str, cols: u16, rows: u16) -> ClientNotification {
    ClientNotification::SessionResize(SessionResize {
        session: sid(session),
        cols,
        rows,
    })
}

fn applied(commands: &[ClientCommand]) -> Vec<LayoutOp> {
    requests(commands)
        .into_iter()
        .filter_map(|r| match &r.body {
            ClientRequest::UiApply(UiApply { op }) => Some(op.clone()),
            _ => None,
        })
        .collect()
}

fn view(cols: u16, rows: u16) -> Area {
    Area {
        x: 0,
        y: 0,
        cols,
        rows,
    }
}

fn root(c: &ClientDriver) -> PaneId {
    c.client().host_layout().current_page().root.id()
}

/// The leaves of the current page, in order.
fn leaves(c: &ClientDriver) -> Vec<PaneId> {
    fn walk(pane: &Pane, out: &mut Vec<PaneId>) {
        match pane {
            Pane::Leaf { id, .. } => out.push(*id),
            Pane::Split { a, b, .. } => {
                walk(a, out);
                walk(b, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&c.client().host_layout().current_page().root, &mut out);
    out
}

/// A client with sessions `s1` and `s2` (selected) in a 100x30 view, its one pane split side by side at halves: the
/// selection on the left, `s1` pinned on the right.
fn split_client() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    let pane = root(&c);
    c.host().ui_apply(LayoutOp::Split {
        pane,
        dir: SplitDir::Horizontal,
        ratio: Ratio::default(),
        content: Content::session(sid("s1")),
    });
    c.sync();
    c
}

// ---------- arrange ----------

#[test]
fn the_default_layout_is_one_borderless_pane_filling_the_view() {
    let arranged = arrange(&Layout::default(), view(80, 24));
    assert_eq!(arranged.page_bar, None);
    assert!(arranged.dividers.is_empty());
    let [pane] = &arranged.panes[..] else {
        panic!("{arranged:?}")
    };
    assert!(!pane.bordered);
    assert_eq!(pane.area, view(80, 24));
    assert_eq!(pane.terminal(), Some(view(80, 24)));
}

#[test]
fn more_than_one_page_takes_a_row_for_the_page_bar() {
    let layout = Layout::default()
        .apply(
            &LayoutOp::PageNew {
                name: None,
                content: Content::Selected,
            },
            |_| true,
        )
        .unwrap();
    let arranged = arrange(&layout, view(80, 24));
    assert_eq!(arranged.page_bar, Some(view(80, 1)));
    let [pane] = &arranged.panes[..] else {
        panic!("{arranged:?}")
    };
    assert_eq!(
        pane.terminal(),
        Some(Area {
            x: 0,
            y: 1,
            cols: 80,
            rows: 23
        })
    );
}

#[test]
fn split_panes_are_bordered_and_share_the_view_by_their_ratio() {
    let layout = Layout::default();
    let pane = layout.current_page().root.id();
    let layout = layout
        .apply(
            &LayoutOp::Split {
                pane,
                dir: SplitDir::Horizontal,
                ratio: Ratio::from_permille(250),
                content: Content::Empty,
            },
            |_| true,
        )
        .unwrap();
    let arranged = arrange(&layout, view(80, 24));
    let [a, b] = &arranged.panes[..] else {
        panic!("{arranged:?}")
    };
    assert!(a.bordered && b.bordered);
    assert_eq!(a.area, view(20, 24));
    assert_eq!(
        b.area,
        Area {
            x: 20,
            y: 0,
            cols: 60,
            rows: 24
        }
    );
    assert_eq!(
        a.terminal(),
        Some(Area {
            x: 1,
            y: 1,
            cols: 18,
            rows: 22
        })
    );
    // The divider is the two border columns where they meet.
    let [divider] = &arranged.dividers[..] else {
        panic!("{arranged:?}")
    };
    assert_eq!(
        divider.cells,
        Area {
            x: 19,
            y: 0,
            cols: 2,
            rows: 24
        }
    );
}

#[test]
fn a_pane_too_small_for_a_terminal_has_none_and_nothing_panics() {
    let mut layout = Layout::default();
    for _ in 0..5 {
        let pane = layout.current_page().root.id();
        layout = layout
            .apply(
                &LayoutOp::Split {
                    pane,
                    dir: SplitDir::Vertical,
                    ratio: Ratio::default(),
                    content: Content::Empty,
                },
                |_| true,
            )
            .unwrap();
    }
    for cols in 0..30 {
        for rows in 0..30 {
            let arranged = arrange(&layout, view(cols, rows));
            for pane in &arranged.panes {
                if let Some(t) = pane.terminal() {
                    assert!(t.cols >= MIN_PANE_COLS && t.rows >= MIN_PANE_ROWS, "{t:?}");
                }
            }
        }
    }
    let tiny = arrange(&layout, view(12, 6));
    assert!(tiny.panes.iter().all(|p| p.terminal().is_none()));
}

#[test]
fn dragging_a_divider_moves_it_by_as_many_cells() {
    let layout = Layout::default();
    let pane = layout.current_page().root.id();
    let layout = layout
        .apply(
            &LayoutOp::Split {
                pane,
                dir: SplitDir::Horizontal,
                ratio: Ratio::default(),
                content: Content::Empty,
            },
            |_| true,
        )
        .unwrap();
    let divider = arrange(&layout, view(80, 24)).dividers[0].clone();
    let split = layout.current_page().root.id();
    assert_eq!(divider.split, split);
    assert_eq!(divider.dragged((40, 5), (40, 9)), None, "not moved");
    let ratio = divider.dragged((40, 5), (50, 5)).unwrap();
    let moved = layout
        .apply(&LayoutOp::Ratio { pane: split, ratio }, |_| true)
        .unwrap();
    assert_eq!(arrange(&moved, view(80, 24)).panes[0].area.cols, 50);
    // Dragged past the edge, it stops short of it.
    let far = divider.dragged((40, 5), (0, 5)).unwrap();
    assert_eq!(far, Ratio::from_permille(Ratio::MIN));
}

// ---------- focus and input ----------

#[test]
fn the_first_pane_is_focused_until_another_is_clicked() {
    let mut c = split_client();
    let [left, right] = leaves(&c)[..] else {
        panic!()
    };
    assert_eq!(c.client().focused_pane(), left);
    assert_eq!(c.client().focused_session(), Some(sid("s2")));
    c.user(Intent::FocusPane(right));
    assert_eq!(c.client().focused_pane(), right);
    assert_eq!(c.client().focused_session(), Some(sid("s1")));
}

#[test]
fn input_goes_to_the_focused_panes_session() {
    let mut c = split_client();
    let right = leaves(&c)[1];
    assert_eq!(
        notifications(&c.user(Intent::keys(b"a".to_vec()))),
        [&input("s2", b"a")]
    );
    c.user(Intent::FocusPane(right));
    assert_eq!(
        notifications(&c.user(Intent::keys(b"b".to_vec()))),
        [&input("s1", b"b")]
    );
}

#[test]
fn input_to_a_pane_showing_no_session_is_dropped() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let pane = root(&c);
    c.host().ui_apply(LayoutOp::Put {
        pane,
        content: Content::View(ViewKind::Status),
    });
    c.sync();
    assert_eq!(c.client().focused_session(), None);
    assert_eq!(c.user(Intent::keys(b"x".to_vec())), []);
}

#[test]
fn focusing_a_pane_resizes_its_session_to_the_pane() {
    let mut c = split_client();
    let right = leaves(&c)[1];
    let sent = c.user(Intent::FocusPane(right));
    // 100 columns at halves: 50 each, less the border.
    assert_eq!(notifications(&sent), [&resize("s1", 48, 28)]);
    assert_eq!(
        c.client().session_size(&sid("s1")),
        Some(PtySize { cols: 48, rows: 28 })
    );
}

#[test]
fn a_layout_change_resizes_the_focused_panes_session() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.user(Intent::Resize(PtySize {
        cols: 100,
        rows: 30,
    }));
    let pane = root(&c);
    c.host().ui_apply(LayoutOp::Split {
        pane,
        dir: SplitDir::Vertical,
        ratio: Ratio::default(),
        content: Content::Empty,
    });
    let sent = c.sync();
    assert_eq!(notifications(&sent), [&resize("s1", 98, 13)]);
}

#[test]
fn selecting_a_session_with_a_pinned_pane_focused_resizes_nothing() {
    let mut c = split_client();
    let right = leaves(&c)[1];
    c.user(Intent::SelectSession(sid("s1")));
    c.user(Intent::FocusPane(right));
    let sent = c.user(Intent::SelectSession(sid("s2")));
    assert_eq!(notifications(&sent), Vec::<&ClientNotification>::new());
}

#[test]
fn focusing_a_pane_too_small_for_a_terminal_resizes_nothing() {
    let mut c = split_client();
    c.user(Intent::Resize(PtySize { cols: 12, rows: 3 }));
    let right = leaves(&c)[1];
    assert_eq!(
        notifications(&c.user(Intent::FocusPane(right))),
        Vec::<&ClientNotification>::new()
    );
    assert_eq!(c.client().focused_pane(), right);
}

#[test]
fn a_pane_not_on_the_current_page_cant_be_focused() {
    let mut c = split_client();
    let left = leaves(&c)[0];
    c.host().ui_apply(LayoutOp::PageNew {
        name: None,
        content: Content::Selected,
    });
    c.sync();
    let new = leaves(&c)[0];
    assert_eq!(c.client().focused_pane(), new);
    c.user(Intent::FocusPane(left));
    assert_eq!(c.client().focused_pane(), new);
}

#[test]
fn a_closed_focused_pane_leaves_focus_on_the_first() {
    let mut c = split_client();
    let [left, right] = leaves(&c)[..] else {
        panic!()
    };
    c.user(Intent::FocusPane(right));
    c.host().ui_apply(LayoutOp::Close { pane: right });
    c.sync();
    assert_eq!(c.client().focused_pane(), left);
}

// ---------- layout ops ----------

#[test]
fn a_layout_op_is_asked_of_the_host() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let op = LayoutOp::PageNew {
        name: None,
        content: Content::Selected,
    };
    let sent = c.user(Intent::ApplyLayout(op.clone()));
    assert_eq!(applied(&sent), [op]);
}

#[test]
fn a_refused_layout_op_is_a_notice() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let sent = c.user(Intent::ApplyLayout(LayoutOp::Close {
        pane: PaneId::new(99),
    }));
    c.answer(
        &sent,
        Err(shepherd_core::wire::WireError::InvalidParams(
            "no pane 99".into(),
        )),
    );
    assert!(
        c.client()
            .notice()
            .is_some_and(|n| n.contains("no pane 99")),
        "{:?}",
        c.client().notice()
    );
}

// ---------- the pane menu ----------

fn open_menu(c: &mut ClientDriver, pane: PaneId) {
    c.user(Intent::OpenPaneMenu {
        pane,
        at: Cell { col: 3, row: 4 },
    });
}

#[test]
fn the_pane_menu_of_a_lone_pane_offers_splits_and_a_new_page() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let pane = root(&c);
    open_menu(&mut c, pane);
    assert_eq!(
        c.client().popup(),
        Some(&Popup::PaneMenu {
            pane,
            at: Cell { col: 3, row: 4 }
        })
    );
    // It already shows the selection, and closing the page's only pane would leave it empty.
    assert_eq!(
        c.client().pane_menu(),
        [
            PaneMenuItem::SplitRight,
            PaneMenuItem::SplitDown,
            PaneMenuItem::NewPage
        ]
    );
}

#[test]
fn the_pane_menu_of_a_split_pane_offers_close_and_move_session_here() {
    let mut c = split_client();
    let right = leaves(&c)[1];
    open_menu(&mut c, right);
    assert_eq!(
        c.client().pane_menu(),
        [
            PaneMenuItem::SplitRight,
            PaneMenuItem::SplitDown,
            PaneMenuItem::Close,
            PaneMenuItem::MoveSessionHere,
            PaneMenuItem::NewPage
        ]
    );
}

#[test]
fn each_pane_menu_item_asks_the_host_for_its_op() {
    let cases = [
        (
            PaneMenuItem::SplitRight,
            LayoutOp::Split {
                pane: PaneId::new(0),
                dir: SplitDir::Horizontal,
                ratio: Ratio::default(),
                content: Content::Empty,
            },
        ),
        (
            PaneMenuItem::SplitDown,
            LayoutOp::Split {
                pane: PaneId::new(0),
                dir: SplitDir::Vertical,
                ratio: Ratio::default(),
                content: Content::Empty,
            },
        ),
        (
            PaneMenuItem::Close,
            LayoutOp::Close {
                pane: PaneId::new(0),
            },
        ),
        (
            PaneMenuItem::MoveSessionHere,
            LayoutOp::Put {
                pane: PaneId::new(0),
                content: Content::session(sid("s2")),
            },
        ),
        (
            PaneMenuItem::NewPage,
            LayoutOp::PageNew {
                name: None,
                content: Content::Selected,
            },
        ),
    ];
    for (item, op) in cases {
        let mut c = split_client();
        let right = leaves(&c)[1];
        let op = match op {
            LayoutOp::Split {
                dir,
                ratio,
                content,
                ..
            } => LayoutOp::Split {
                pane: right,
                dir,
                ratio,
                content,
            },
            LayoutOp::Close { .. } => LayoutOp::Close { pane: right },
            LayoutOp::Put { content, .. } => LayoutOp::Put {
                pane: right,
                content,
            },
            op => op,
        };
        open_menu(&mut c, right);
        let sent = c.user(Intent::PaneMenuChoose(item));
        assert_eq!(applied(&sent), [op], "{item:?}");
        assert_eq!(c.client().popup(), None);
    }
}

#[test]
fn closing_a_page_is_offered_while_there_is_another() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    c.host().ui_apply(LayoutOp::PageNew {
        name: None,
        content: Content::Selected,
    });
    c.sync();
    let pane = root(&c);
    let page = c.client().host_layout().current();
    open_menu(&mut c, pane);
    assert!(c.client().pane_menu().contains(&PaneMenuItem::ClosePage));
    let sent = c.user(Intent::PaneMenuChoose(PaneMenuItem::ClosePage));
    assert_eq!(applied(&sent), [LayoutOp::PageClose { page }]);
}

#[test]
fn an_item_the_menu_does_not_offer_does_nothing() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let pane = root(&c);
    open_menu(&mut c, pane);
    assert_eq!(c.user(Intent::PaneMenuChoose(PaneMenuItem::Close)), []);
}

#[test]
fn a_layout_change_that_leaves_the_focused_pane_alone_resizes_nothing() {
    // Another window may have focused this pane since: only it should size the session now.
    let mut c = split_client();
    let page = c.client().host_layout().current();
    c.host().ui_apply(LayoutOp::PageRename {
        page,
        name: "work".into(),
    });
    let sent = c.sync();
    assert_eq!(notifications(&sent), Vec::<&ClientNotification>::new());
}

#[test]
fn a_pane_menu_closes_when_the_host_closes_its_pane() {
    let mut c = split_client();
    let right = leaves(&c)[1];
    open_menu(&mut c, right);
    c.host().ui_apply(LayoutOp::Close { pane: right });
    c.sync();
    assert_eq!(c.client().popup(), None);
    assert_eq!(c.client().pane_menu(), []);
}

#[test]
fn a_menu_is_opened_only_on_a_pane_of_the_current_page_that_isnt_split() {
    let mut c = split_client();
    let split = root(&c);
    open_menu(&mut c, split);
    assert_eq!(c.client().popup(), None, "a split");
    open_menu(&mut c, PaneId::new(999));
    assert_eq!(c.client().popup(), None, "no such pane");
    let left = leaves(&c)[0];
    c.host().ui_apply(LayoutOp::PageNew {
        name: None,
        content: Content::Selected,
    });
    c.sync();
    open_menu(&mut c, left);
    assert_eq!(c.client().popup(), None, "another page's");
}
