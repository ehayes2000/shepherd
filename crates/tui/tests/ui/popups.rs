//! Dialogs and the profile picker, answered by clicking their buttons and rows. Ported from `mouse.rs` (T45).

use shepherd_core::client::Popup;
use shepherd_core::config::AgentKind;
use shepherd_core::wire::ClientRequest;
use shepherd_testkit::builders::ClientDriver;

use crate::harness::Ui;

/// `name-01`'s close confirmation, opened by its row's close button.
fn confirming_close() -> Ui {
    let mut ui = Ui::with_sessions(&["name-01"], 80, 24);
    ui.click_in(ui.line_of("name-01"), "×");
    ui
}

fn closes(ui: &Ui) -> usize {
    ui.requests()
        .iter()
        .filter(|r| matches!(r, ClientRequest::SessionClose(_)))
        .count()
}

#[test]
fn the_close_confirmation_answers_by_its_buttons() {
    let mut ui = confirming_close();
    ui.click("[Close]");
    assert_eq!(closes(&ui), 1, "{:?}", ui.requests());
    assert_eq!(ui.client().popup(), None);

    let mut ui = confirming_close();
    ui.click("[Cancel]");
    assert_eq!(closes(&ui), 0, "{:?}", ui.requests());
    assert_eq!(ui.client().popup(), None);
}

#[test]
fn clicking_outside_a_dialog_cancels_it() {
    let mut ui = Ui::with_sessions(&["name-01"], 80, 24);
    ui.right_click("name-01").click("Rename");
    ui.assert_contains("[Save]");
    // A control under the backdrop only dismisses the dialog.
    ui.click("+ New session");
    assert_eq!(ui.client().popup(), None);
    ui.assert_lacks("[Save]");
    assert_eq!(ui.requests(), [] as [&ClientRequest; 0]);
}

/// A connected client with session `name-01`, whose host has the `claude`, `codex` and `shell` profiles, `codex` the
/// default, with the picker opened by clicking `+ New session` (`+` when collapsed).
fn picker(cols: u16, rows: u16) -> Ui {
    let mut c = ClientDriver::new();
    c.connect_with_profiles(
        &[
            ("claude", AgentKind::ClaudeCode),
            ("codex", AgentKind::Codex),
            ("shell", AgentKind::Shell),
        ],
        "codex",
    );
    c.host().new_session("s1");
    c.host().rename("s1", "name-01");
    c.sync();
    let mut ui = Ui::new(c, cols, rows);
    ui.click_in(ui.line_of("+"), "+");
    assert_eq!(ui.client().popup(), Some(&Popup::PickProfile));
    ui
}

#[test]
fn clicking_a_profile_creates_a_session_with_it() {
    for ((cols, rows), name) in [
        ((80, 24), "shell"),
        ((80, 24), "codex"),
        ((40, 12), "claude"),
    ] {
        let mut ui = picker(cols, rows);
        // The name column; the agent column after it can read the same.
        let row = ui.find_in(ui.boxed("new session"), name)[0];
        ui.click_at(row.x, row.y);
        let profiles: Vec<Option<&str>> = ui
            .requests()
            .iter()
            .filter_map(|r| match r {
                ClientRequest::SessionNew(new) => Some(new.profile.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(profiles, [Some(name)], "{name}");
        assert_eq!(ui.client().popup(), None, "{name}");
    }
}
