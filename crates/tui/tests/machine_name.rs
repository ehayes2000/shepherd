//! The machine on a session's second sidebar line (T29): the host's name for its machine, `studio · #123 ✓`.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use shepherd_core::client::Client;
use shepherd_testkit::builders::ClientDriver;
use shepherd_tui::{Screens, render};

fn buffer(client: &Client, cols: u16, rows: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(client, &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

/// The line under the session called `name` in the sidebar (`width` columns), trimmed.
fn detail(buf: &Buffer, width: u16, name: &str) -> String {
    let text: Vec<String> = (0..buf.area.height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    let at = text
        .iter()
        .position(|l| l.contains(name))
        .unwrap_or_else(|| panic!("{name} not in {text:#?}"));
    text[at + 1].trim_end_matches([' ', '│']).trim().to_owned()
}

/// A host called `host_name`, with a session from before machines had ids (`old`, stamped `local`), one from after
/// (`new`), and one another machine recorded (`elsewhere`).
fn mixed(host_name: &str) -> ClientDriver {
    let mut c = ClientDriver::new();
    c.connect_named(host_name);
    for (machine, id) in [
        ("local", "old"),
        ("m_test", "new"),
        ("m_other", "elsewhere"),
    ] {
        c.host().stamp_machine(machine);
        c.host().new_session(id);
        c.host().rename(id, id);
    }
    c.sync();
    c
}

#[test]
fn sidebar_with_the_machine_named() {
    let c = mixed("studio");
    insta::assert_debug_snapshot!(buffer(c.client(), 80, 11));
}

#[test]
fn line_two_shows_the_hosts_machine_name_for_old_and_new_sessions() {
    let c = mixed("studio");
    let buf = buffer(c.client(), 80, 12);
    assert_eq!(detail(&buf, 24, "old"), "studio · —");
    assert_eq!(detail(&buf, 24, "new"), "studio · —");
    assert_eq!(detail(&buf, 24, "elsewhere"), "m_other · —");
}

#[test]
fn a_machine_name_is_drawn_display_safe() {
    let c = mixed("\u{1b}[31mev\u{202e}il");
    let buf = buffer(c.client(), 80, 12);
    assert_eq!(detail(&buf, 24, "new"), "[31mevil · —");
}
