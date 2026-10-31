//! The host's layout drawn and driven by clicking (T49): the page bar, split panes with titled borders, focus by click,
//! dragging a divider, and each pane's menu.

use ratatui::layout::{Position, Rect};
use shepherd_core::client::ClientCommand;
use shepherd_core::client::Intent;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{Content, LayoutOp, Pane, PaneId, Ratio, SplitDir, ViewKind};
use shepherd_core::wire::{ClientMessage, ClientNotification, SessionInput};

use crate::harness::Ui;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

/// The leaves of the current page, in order.
fn leaves(ui: &Ui) -> Vec<PaneId> {
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
    walk(&ui.client().host_layout().current_page().root, &mut out);
    out
}

fn root(ui: &Ui) -> PaneId {
    ui.client().host_layout().current_page().root.id()
}

fn split(pane: PaneId, dir: SplitDir, content: Content) -> LayoutOp {
    LayoutOp::Split {
        pane,
        dir,
        ratio: Ratio::default(),
        content,
    }
}

/// Sessions `fix-login` (`s1`), `docs` (`s2`) and `api` (`s3`, selected), each having printed its name, with the page
/// split three ways: the selection on the left, `fix-login` top right, `docs` bottom right.
fn three_way(cols: u16, rows: u16) -> Ui {
    let mut ui = Ui::with_sessions(&["fix-login", "docs", "api"], cols, rows);
    let pane = root(&ui);
    ui.host_layout(split(
        pane,
        SplitDir::Horizontal,
        Content::session(sid("s1")),
    ));
    let right = leaves(&ui)[1];
    ui.host_layout(split(
        right,
        SplitDir::Vertical,
        Content::session(sid("s2")),
    ));
    for (id, text) in [("s1", "in fix-login"), ("s2", "in docs"), ("s3", "in api")] {
        ui.pty(id, text.as_bytes());
    }
    ui
}

/// The input notifications the client has sent, as (session, bytes).
fn inputs(ui: &Ui) -> Vec<(String, Vec<u8>)> {
    ui.sent()
        .iter()
        .filter_map(|c| match c {
            ClientCommand::Send(ClientMessage::Notification(ClientNotification::SessionInput(
                SessionInput { session, data, .. },
            ))) => Some((session.to_string(), data.clone())),
            _ => None,
        })
        .collect()
}

// ---------- drawing ----------

#[test]
fn one_page_of_one_pane_looks_as_before_layouts() {
    let mut ui = Ui::with_sessions(&["fix-login"], 60, 8);
    ui.pty("s1", b"hello");
    ui.assert_lacks("[≡]").assert_lacks("┌");
    // The session's terminal starts at the pane area's top-left corner.
    let at = ui.at("hello");
    assert_eq!(at.y, 0);
}

#[test]
fn two_pages() {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], 80, 10);
    ui.host_layout(LayoutOp::PageNew {
        name: Some("review".into()),
        content: Content::Selected,
    });
    ui.pty("s2", b"in docs");
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn a_three_way_split() {
    let ui = three_way(100, 16);
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn a_three_way_split_in_a_tiny_window() {
    let ui = three_way(40, 6);
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn views_and_empty_panes_are_titled_placeholders() {
    let mut ui = Ui::with_sessions(&["fix-login"], 100, 12);
    let pane = root(&ui);
    ui.host_layout(split(
        pane,
        SplitDir::Horizontal,
        Content::View(ViewKind::Status),
    ));
    let left = leaves(&ui)[0];
    ui.host_layout(split(left, SplitDir::Vertical, Content::Empty));
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn a_pinned_peers_machine_and_an_unknown_views_name_are_drawn_safe_and_clipped() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 10);
    let evil = format!("\u{1b}[31mevil\u{7}{}", "m".repeat(200));
    let pane = root(&ui);
    ui.host_layout(split(
        pane,
        SplitDir::Horizontal,
        Content::peer_session(MachineId::new(&evil), sid("p1")),
    ));
    let left = leaves(&ui)[0];
    ui.host_layout(LayoutOp::Put {
        pane: left,
        content: Content::View(ViewKind::Other(format!(
            "\u{1b}]0;x\u{7}{}",
            "v".repeat(200)
        ))),
    });
    let screen = ui.screen();
    assert!(
        !screen.contains('\u{1b}') && !screen.contains('\u{7}'),
        "{screen}"
    );
    // Each title stays on its pane's top edge, ending before the pane's menu button.
    let top = ui.region(Rect::new(0, 0, 80, 1));
    assert_eq!(top.matches("[≡]").count(), 2, "{top}");
    assert!(top.contains("p1 · [31mevil"), "{top}");
    insta::assert_snapshot!(screen);
}

#[test]
fn a_peers_pinned_session_it_cant_reach_says_so() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 10);
    let pane = root(&ui);
    ui.host_layout(split(
        pane,
        SplitDir::Horizontal,
        Content::peer_session(MachineId::new("m_far"), sid("p1")),
    ));
    ui.assert_contains("can't be reached");
}

// ---------- focus ----------

#[test]
fn clicking_a_pane_focuses_it_and_keys_go_to_its_session() {
    let mut ui = three_way(100, 16);
    let [_, top_right, _] = leaves(&ui)[..] else {
        panic!()
    };
    assert_ne!(ui.client().focused_pane(), top_right);
    let inside = ui.boxed("fix-login");
    ui.click_in(inside, "in fix-login");
    assert_eq!(ui.client().focused_pane(), top_right);
    ui.type_text("x");
    assert_eq!(inputs(&ui).last(), Some(&("s1".to_owned(), b"x".to_vec())));
}

#[test]
fn a_focused_pane_s_session_is_resized_to_its_pane() {
    let mut ui = three_way(100, 16);
    let inside = ui.boxed("docs");
    ui.click_in(inside, "in docs");
    let screen = ui.screen_of("s2").unwrap();
    assert_eq!(screen.size(), (inside.width, inside.height));
}

// ---------- dragging a divider ----------

#[test]
fn dragging_the_border_between_two_panes_sets_their_split_s_ratio() {
    let mut ui = three_way(100, 16);
    let left = ui.boxed("api");
    // The left pane's right border, below its title.
    let border = Position::new(left.right(), left.y + 2);
    ui.drag(border, Position::new(border.x + 10, border.y));
    let ops = ui.layout_ops();
    assert!(
        matches!(ops.last(), Some(LayoutOp::Ratio { pane, .. }) if *pane == root(&ui)),
        "{ops:?}"
    );
    ui.apply_layout_ops();
    assert_eq!(ui.boxed("api").width, left.width + 10, "{}", ui.screen());
}

#[test]
fn a_click_on_a_border_without_dragging_changes_nothing() {
    let mut ui = three_way(100, 16);
    let left = ui.boxed("api");
    ui.click_at(left.right(), left.y + 2);
    assert_eq!(ui.layout_ops(), []);
}

// ---------- the pane menu and the page bar ----------

#[test]
fn right_clicking_the_one_pane_opens_its_menu_and_split_right_splits_it() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 10);
    ui.pty("s1", b"hello");
    ui.right_click("hello");
    ui.assert_contains("Split right")
        .assert_contains("New page");
    ui.click("Split right");
    ui.apply_layout_ops();
    assert_eq!(leaves(&ui).len(), 2);
    ui.boxed("fix-login");
    ui.boxed("empty");
}

/// Clicks the menu button of the pane titled `title`.
fn open_menu(ui: &mut Ui, title: &str) {
    let inside = ui.boxed(title);
    let top = Rect::new(inside.x - 1, inside.y - 1, inside.width + 2, 1);
    ui.click_in(top, "[≡]");
}

#[test]
fn each_pane_menu_op_applies_through_the_host() {
    let mut ui = three_way(100, 16);
    open_menu(&mut ui, "fix-login");
    ui.click("Split down");
    ui.apply_layout_ops();
    assert_eq!(leaves(&ui).len(), 4, "{}", ui.screen());
    open_menu(&mut ui, "empty");
    ui.click("Close pane");
    ui.apply_layout_ops();
    assert_eq!(leaves(&ui).len(), 3, "{}", ui.screen());
    // The selected session, api, pinned in the docs pane.
    open_menu(&mut ui, "docs");
    ui.click("Move session here");
    ui.apply_layout_ops();
    let docs_pane = leaves(&ui)[2];
    assert_eq!(
        ui.client().host_layout().pane(docs_pane),
        Some(&Pane::Leaf {
            id: docs_pane,
            content: Content::session(sid("s3")),
        })
    );
    open_menu(&mut ui, "fix-login");
    ui.click("Split right");
    ui.apply_layout_ops();
    assert_eq!(leaves(&ui).len(), 4, "{}", ui.screen());
    assert_eq!(ui.client().notice(), None);
}

#[test]
fn the_page_bar_selects_pages_and_adds_one() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 10);
    ui.host_layout(LayoutOp::PageNew {
        name: Some("review".into()),
        content: Content::Selected,
    });
    let first = ui.client().host_layout().pages()[0].id;
    ui.click(" 1 ");
    assert_eq!(ui.layout_ops(), [LayoutOp::PageSelect { page: first }]);
    ui.apply_layout_ops();
    assert_eq!(ui.client().host_layout().current(), first);
    ui.click_in(Rect::new(24, 0, 56, 1), "+");
    ui.apply_layout_ops();
    assert_eq!(ui.client().host_layout().pages().len(), 3);
}

#[test]
fn new_page_from_the_menu_shows_the_page_bar() {
    let mut ui = Ui::with_sessions(&["fix-login"], 80, 10);
    ui.pty("s1", b"hello");
    ui.right_click("hello");
    ui.click("New page");
    ui.apply_layout_ops();
    ui.assert_contains(" 2 ");
    // The selection shows on the new page too, a row lower.
    assert_eq!(ui.at("hello").y, 1);
    ui.user(Intent::SelectSession(sid("s1")));
}
