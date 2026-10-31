//! The sidebar, clicked where it is drawn. Ported from `mouse.rs` (T45).

use shepherd_core::client::{ClientCommand, Focus, Popup};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::ClientRequest;

use crate::harness::Ui;

#[test]
fn clicking_a_row_selects_it_and_focuses_the_terminal() {
    let mut ui = Ui::with_sessions(&["name-01", "name-02"], 80, 24);
    ui.ctrl('b');
    assert_eq!(ui.client().focus(), Focus::Command);
    let row = ui.at("name-01");
    // Either line of the row.
    ui.click_at(row.x, row.y + 1);
    assert_eq!(ui.client().selected(), Some(&SessionId::new("s1")));
    assert_eq!(ui.client().focus(), Focus::Terminal);
}

#[test]
fn clicking_a_rows_close_button_asks_to_close_it_without_selecting_it() {
    let mut ui = Ui::with_sessions(&["name-01", "name-02"], 80, 24);
    ui.click_in(ui.line_of("name-01"), "×");
    ui.assert_contains("Close name-01?");
    assert_eq!(
        ui.client().popup(),
        Some(&Popup::ConfirmClose {
            session: SessionId::new("s1")
        })
    );
    assert_eq!(ui.client().selected(), Some(&SessionId::new("s2")));
}

#[test]
fn clicking_new_session_in_the_collapsed_sidebar_starts_one() {
    let mut ui = Ui::with_sessions(&["name-01"], 40, 12);
    ui.assert_lacks("New session");
    ui.click("+");
    assert!(
        matches!(ui.requests()[..], [ClientRequest::SessionNew(_)]),
        "{:?}",
        ui.requests()
    );
}

#[test]
fn clicking_detach_quits_and_clicking_help_shows_it() {
    let mut ui = Ui::with_sessions(&["name-01"], 80, 24);
    ui.click("Detach");
    assert_eq!(ui.sent().last(), Some(&ClientCommand::Quit));
    // The footer's `?` button is its last cell; the hint before it says `ctrl-b ? help`.
    let help = *ui.find("?").last().expect("the footer's ? button");
    ui.click_at(help.x, help.y);
    assert_eq!(ui.client().popup(), Some(&Popup::Help));
}
