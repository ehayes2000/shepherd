//! A session's pull request on its second sidebar line (T27): `local · #123 ✓`.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use serde_json::{Value, json};
use shepherd_core::acpp::{Source, SourceKind};
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

/// The sidebar's text, line by line, right spaces trimmed.
fn sidebar_text(buf: &Buffer, width: u16) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let line: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            line.trim_end().to_owned()
        })
        .collect()
}

fn session_pr(c: &mut ClientDriver, session: &str, params: Value) {
    let frame = json!({ "jsonrpc": "2.0", "method": "_shepherd/session/pr", "params": params });
    c.host().append(
        Some(session),
        Source::new(SourceKind::Shepherd, None),
        frame,
    );
}

fn open(number: u64, checks: &str) -> Value {
    json!({ "pr": { "number": number, "state": "open", "checks": checks } })
}

/// Seven sessions, newest first: PRs with passing, failing, pending and no checks, a merged and a closed one, one
/// in a state this build doesn't know (with an escape sequence in it, which never reaches the screen), and one with
/// no PR.
fn sessions_with_prs() -> ClientDriver {
    let ids = [
        "none",
        "evil",
        "closed",
        "merged",
        "no-checks",
        "pending",
        "failing",
        "passing",
    ];
    let mut c = ClientDriver::with_sessions(&ids);
    for id in ids {
        c.host().rename(id, id);
    }
    session_pr(&mut c, "passing", open(123, "passing"));
    session_pr(&mut c, "failing", open(124, "failing"));
    session_pr(&mut c, "pending", open(125, "pending"));
    session_pr(&mut c, "no-checks", open(126, "none"));
    session_pr(
        &mut c,
        "merged",
        json!({ "pr": { "number": 127, "state": "merged", "checks": "passing" } }),
    );
    session_pr(
        &mut c,
        "closed",
        json!({ "pr": { "number": 128, "state": "closed", "checks": "failing" } }),
    );
    session_pr(
        &mut c,
        "evil",
        json!({ "pr": { "number": 129, "state": "\u{1b}[2Jq", "checks": "passing" } }),
    );
    c.sync();
    c
}

#[test]
fn sessions_with_their_prs() {
    let c = sessions_with_prs();
    insta::assert_debug_snapshot!(buffer(c.client(), 80, 21));
}

#[test]
fn line_two_shows_the_pr_number_and_its_checks_after_the_machine() {
    let c = sessions_with_prs();
    let text = sidebar_text(&buffer(c.client(), 80, 21), 24);
    let detail = |name: &str| {
        let at = text
            .iter()
            .position(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} not in {text:#?}"));
        text[at + 1].trim().trim_end_matches('│').trim().to_owned()
    };
    assert_eq!(detail("passing"), "local · #123 ✓");
    assert_eq!(detail("failing"), "local · #124 ✗");
    assert_eq!(detail("pending"), "local · #125 …");
    assert_eq!(detail("no-checks"), "local · #126");
    assert_eq!(detail("merged"), "local · #127 merged");
    assert_eq!(detail("closed"), "local · #128 closed");
    assert_eq!(detail("evil"), "local · #129 [2Jq");
    assert_eq!(detail("none"), "local · —");
}
