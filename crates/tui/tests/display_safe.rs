//! Text from agents, hooks, git and other clients is drawn display-safe everywhere it shows (F35): no control or
//! format characters reach the screen, and no cell stacks more than two combining marks on a base.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use shepherd_core::client::{ClientEvent, Intent};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::WorktreeError;
use shepherd_core::wire::WireError;
use shepherd_testkit::builders::{ClientDriver, ts};
use shepherd_tui::{Screens, render};
use unicode_normalization::char::is_combining_mark;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Visible `start` and `end` around a Trojan Source override and isolate, joiners, a BOM, a tag, a soft hyphen, zalgo
/// marks, and escape and bell controls.
fn evil(start: &str, end: &str) -> String {
    format!(
        "{start}\u{202E}\u{2066}x\u{200D}\u{FEFF}\u{E0041}\u{AD}a\u{301}\u{302}\u{303}\u{304}\x1b\x07\u{9b}{end}"
    )
}

fn buffer(c: &ClientDriver, cols: u16, rows: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap_or_else(|e| match e {});
    terminal
        .draw(|frame| render(c.client(), &Screens::new(), frame))
        .unwrap_or_else(|e| match e {});
    terminal.backend().buffer().clone()
}

fn invisible(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::Control | GeneralCategory::Format
    )
}

/// Every cell of `buf` that shows a control or format character, or more than two combining marks after a base.
fn unsafe_cells(buf: &Buffer) -> Vec<(u16, u16, String)> {
    let mut found = Vec::new();
    for y in buf.area.top()..buf.area.bottom() {
        for x in buf.area.left()..buf.area.right() {
            let symbol = buf[(x, y)].symbol();
            let mut marks = 0;
            let mut stacked = false;
            for c in symbol.chars() {
                if is_combining_mark(c) {
                    marks += 1;
                    stacked |= marks > 2;
                } else {
                    marks = 0;
                }
            }
            if stacked || symbol.chars().any(invisible) {
                found.push((x, y, format!("{symbol:?}")));
            }
        }
    }
    found
}

/// The buffer's text, a line per row.
fn text(buf: &Buffer) -> String {
    let mut lines = Vec::new();
    for y in buf.area.top()..buf.area.bottom() {
        let line: String = (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        lines.push(line.trim_end().to_owned());
    }
    lines.join("\n")
}

/// Sessions whose name, failure reason, hook warning and workspace are all evil: `s1` in another (evil) workspace,
/// `s2` failed to make its worktree, `s3` started with a failed hook, `s4` (newest, selected) only has an evil name.
fn evil_sidebar() -> ClientDriver {
    let mut c = ClientDriver::with_sessions(&[]);
    c.host().select_workspace(&evil("/src/el", "sewhere"));
    c.host().new_session("s1");
    c.host().rename("s1", &evil("away", ""));
    c.host().worktree_ready("s1");
    c.host().select_workspace(&evil("/src/sh", "ep"));
    c.host().new_session("s2");
    c.host().rename("s2", &evil("fail", "ed"));
    let error = WorktreeError::FetchFailed(evil("could not ", "resolve host"));
    c.host().worktree_result("s2", Err(error));
    c.host().new_session("s3");
    c.host().rename("s3", &evil("hoo", "ked"));
    let script = evil("cp ", ".envrc .");
    c.host().worktree_ready_with_hooks("s3", &[&script]);
    c.host().hook_exited("s3", &script, 1);
    c.host().new_session("s4");
    c.host().rename("s4", &evil("ren", "amed"));
    c.host().worktree_ready("s4");
    c.sync();
    c
}

/// Every state that draws text from outside shepherd, with the evil text's visible part that it shows.
fn evil_states() -> Vec<(&'static str, ClientDriver, &'static str)> {
    let mut states = vec![("sidebar", evil_sidebar(), "renx")];

    let mut notice = evil_sidebar();
    notice.user(Intent::BeginRename);
    let sent = notice.user(Intent::Confirm);
    notice.answer(&sent, Err(WireError::InvalidParams(evil("no ", "pe"))));
    states.push(("notice", notice, "no x"));

    let mut rename = evil_sidebar();
    rename.user(Intent::BeginRename);
    rename.user(Intent::EditRename(evil("typ", "ed")));
    states.push(("rename input", rename, "typx"));

    let mut close = evil_sidebar();
    close.user(Intent::BeginClose);
    states.push(("close confirmation", close, "Close renx"));

    let mut unchecked = evil_sidebar();
    let asked = unchecked.user(Intent::BeginCloseSession(SessionId::new("s4")));
    unchecked.answer(
        &asked,
        Err(WireError::InvalidParams(evil("not ", "a worktree"))),
    );
    states.push((
        "close confirmation that could not check",
        unchecked,
        "not x",
    ));

    let mut switcher = evil_sidebar();
    switcher.user(Intent::BeginWorkspace);
    switcher.user(Intent::EditWorkspacePath(evil("/typ", "ed")));
    states.push(("workspace switcher", switcher, "/typx"));

    let mut refused = evil_sidebar();
    refused.user(Intent::BeginWorkspace);
    refused.user(Intent::EditWorkspacePath("/tmp/x".into()));
    let sent = refused.user(Intent::Confirm);
    refused.answer(&sent, Err(WireError::InvalidParams(evil("not ", "a repo"))));
    states.push(("workspace switcher with an error", refused, "not x"));

    let mut picker = ClientDriver::new();
    let name = evil("pro", "file");
    picker.connect_with_profiles(
        &[(&name, AgentKind::ClaudeCode), ("sh", AgentKind::Shell)],
        &name,
    );
    picker.user(Intent::BeginNewSession);
    states.push(("profile picker", picker, "prox"));

    let mut failed = ClientDriver::new();
    failed.handle(ClientEvent::Start);
    let hello = failed.handle(ClientEvent::Connected {
        at: ts("2026-10-29T12:00:00.000Z"),
    });
    failed.answer(&hello, Err(WireError::InvalidParams(evil("go ", "away"))));
    states.push(("connection failed", failed, "go x"));

    let mut info = evil_sidebar();
    info.user(Intent::OpenMenu {
        session: SessionId::new("s4"),
        at: shepherd_core::client::Cell { col: 5, row: 4 },
    });
    states.push(("menu", info, "Rename"));
    states
}

#[test]
fn text_from_outside_shepherd_is_drawn_display_safe_everywhere() {
    let mut drawn_unsafe = Vec::new();
    for (name, c, shows) in evil_states() {
        let wide = text(&buffer(&c, 100, 24));
        assert!(
            wide.contains(shows),
            "{name} doesn't show {shows:?}:\n{wide}"
        );
        for (cols, rows) in [(100, 24), (40, 12)] {
            let cells = unsafe_cells(&buffer(&c, cols, rows));
            if !cells.is_empty() {
                drawn_unsafe.push(format!("{name} at {cols}x{rows}: {cells:?}"));
            }
        }
    }
    assert_eq!(drawn_unsafe, Vec::<String>::new());
}

/// What the evil sidebar looks like once it's safe: the visible text is all still there.
#[test]
fn an_evil_sidebar() {
    insta::assert_snapshot!(text(&buffer(&evil_sidebar(), 100, 12)));
}
