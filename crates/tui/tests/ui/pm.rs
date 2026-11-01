//! The project manager's page (T60): `ctrl-b p` or the pane menu's "Project manager" starts the PM session (the `pm`
//! profile) and opens its page, the PM on the left and the PM panel (a placeholder until T61) on the right; the page
//! bar selects it like any page; the PM is an ordinary row in the sidebar.

use ratatui::layout::Rect;
use serde_json::json;
use shepherd_core::config::PM_PROFILE;
use shepherd_core::layout::{Content, LayoutOp, Pane, ViewKind};
use shepherd_core::wire::{ClientRequest, Method};
use shepherd_testkit::builders::pm_session_spec;
use shepherd_tui::layout::layout;

use crate::harness::Ui;

/// `fix-login` and `docs`, in a `cols` x `rows` window.
fn two_sessions(cols: u16, rows: u16) -> Ui {
    let mut ui = Ui::with_sessions(&["fix-login", "docs"], cols, rows);
    ui.pty("s2", b"in docs");
    ui
}

/// The host starts PM session `pm1` the client asked for, as it answers a `session/new`, and it prints its prompt.
fn host_starts_the_pm(ui: &mut Ui) {
    assert_eq!(ui.unanswered(Method::SessionNew), 1, "{:?}", ui.requests());
    let profile = ui.requests().into_iter().find_map(|r| match r {
        ClientRequest::SessionNew(new) => new.profile.clone(),
        _ => None,
    });
    assert_eq!(profile.as_deref(), Some(PM_PROFILE));
    ui.answer(Method::SessionNew, Ok(json!({ "session": "pm1" })));
    ui.on_host(|h| {
        h.new_session_with("pm1", pm_session_spec());
    });
    ui.pty("pm1", b"Claude Code\r\n> what should we work on?");
}

/// The page bar: the first row of the pane area.
fn page_bar(ui: &Ui) -> Rect {
    let pane = layout(ui.window()).pane;
    Rect { height: 1, ..pane }
}

#[test]
fn ctrl_b_p_starts_the_pm_and_opens_its_page() {
    let mut ui = two_sessions(100, 16);
    ui.ctrl('b').type_text("p");
    host_starts_the_pm(&mut ui);
    ui.apply_layout_ops();
    let page = ui.client().host_layout().current_page().clone();
    assert_eq!(page.name, "pm");
    let Pane::Split { a, b, .. } = &page.root else {
        panic!("not split: {:?}", page.root);
    };
    assert!(matches!(
        **a,
        Pane::Leaf {
            content: Content::Session { .. },
            ..
        }
    ));
    assert!(matches!(
        &**b,
        Pane::Leaf {
            content: Content::View(ViewKind::PmPanel),
            ..
        }
    ));
    // The PM has its own row in the sidebar, and takes the keys.
    assert_eq!(
        ui.client().focused_session().map(|s| s.as_str().to_owned()),
        Some("pm1".to_owned())
    );
    ui.assert_contains("pm-3")
        .assert_contains("what should we work on?");
    insta::assert_snapshot!(ui.screen());
}

#[test]
fn ctrl_b_p_shows_the_existing_pm_and_the_page_bar_selects_it() {
    let mut ui = two_sessions(100, 16);
    ui.ctrl('b').type_text("p");
    host_starts_the_pm(&mut ui);
    ui.apply_layout_ops();
    let pm_page = ui.client().host_layout().current();
    let bar = page_bar(&ui);
    ui.click_in(bar, "1").apply_layout_ops();
    assert_ne!(ui.client().host_layout().current(), pm_page);
    ui.assert_lacks("Project manager isn't");

    // The page bar selects it like any page.
    ui.click_in(bar, "pm").apply_layout_ops();
    assert_eq!(ui.client().host_layout().current(), pm_page);

    // From another page, `ctrl-b p` selects it again and starts nothing.
    ui.click_in(bar, "1").apply_layout_ops();
    ui.ctrl('b').type_text("p");
    assert_eq!(
        ui.layout_ops().last(),
        Some(&LayoutOp::PageSelect { page: pm_page })
    );
    ui.apply_layout_ops();
    assert_eq!(ui.client().host_layout().current(), pm_page);
    assert_eq!(ui.unanswered(Method::SessionNew), 0);
    assert_eq!(ui.client().host_layout().pages().len(), 2);
}

#[test]
fn the_pane_menu_opens_the_project_manager() {
    let mut ui = two_sessions(100, 16);
    ui.click("⊞ Layout").click("Project manager");
    host_starts_the_pm(&mut ui);
    ui.apply_layout_ops();
    assert_eq!(ui.client().host_layout().current_page().name, "pm");
    ui.assert_contains("Project manager");
}
