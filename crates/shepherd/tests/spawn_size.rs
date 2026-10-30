//! A new session's PTY spawns at the size of the client's view, so its first frame needs no resize (F42): the real
//! client runtime against the real host runtime on all-fake ports. The workspace is a real directory in a tempdir,
//! and the test makes the worktree's directory itself, standing in for git.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::Event;
use shepherd::client::{ClientRuntime, Connector, TerminalView, View};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{Connection, Intent};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::pty::PtySize;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::FakeLink;
use shepherd_testkit::fakes::pty::ProgramInput;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use shepherd_tui::{layout, pane_size};
use support::*;

/// A liveness bound for the held git call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);

const WINDOW: (u16, u16) = (120, 40);

/// What the fake shell saw: the size it started at, and each size it was resized to after. The fake PTY signals a
/// resize to the size it already has too, which a real one doesn't.
#[derive(Debug, Default)]
struct Seen {
    started: Vec<PtySize>,
    resized: Vec<PtySize>,
}

/// A shell that fills its first row with `=`, as wide as its terminal, and records what it saw.
fn ruler_shell(seen: &Arc<Mutex<Seen>>) -> Program {
    let seen = seen.clone();
    Arc::new(move |input, p| match input {
        ProgramInput::Start => {
            let size = p.size();
            seen.lock().unwrap().started.push(size);
            p.print("=".repeat(usize::from(size.cols)));
        }
        ProgramInput::Resized => seen.lock().unwrap().resized.push(p.size()),
        ProgramInput::Stdin(_) => {}
    })
}

struct Setup {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    worktrees: FakeWorktrees,
    h: HostHarness,
    client: ClientRuntime<FakeLink, Connector<FakeLink>, FakeHostLauncher>,
    seen: Arc<Mutex<Seen>>,
}

impl Setup {
    /// Pumps host and client until neither has anything left to do. With `wait`, also until no worker call is
    /// unanswered.
    fn settle(&mut self, wait: bool) {
        loop {
            if wait {
                self.h.step();
            } else {
                self.h.poll();
            }
            if self.client.pump() == 0 {
                return;
            }
        }
    }

    fn term(&mut self, event: Event, wait: bool) {
        assert!(self.client.events().terminal(event));
        self.settle(wait);
    }

    /// The client's frame, drawn in a `WINDOW`-sized terminal.
    fn frame(&self) -> TerminalView<TestBackend> {
        let backend = ratatui::Terminal::new(TestBackend::new(WINDOW.0, WINDOW.1))
            .unwrap_or_else(|e| match e {});
        let mut view = TerminalView::new(backend);
        view.draw(self.client.client(), self.client.screens())
            .unwrap();
        view
    }
}

/// A host whose workspace is `<tempdir>/app`, a repo the fake port knows, and a client started there in a
/// `WINDOW`-sized terminal, connected.
fn setup() -> Setup {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let app = base.join("app");
    let root = base.join("worktrees");
    std::fs::create_dir(&app).unwrap();
    let worktrees = FakeWorktrees::new(&root);
    worktrees.add_repo(&app, "main");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let h = HostHarness::builder()
        .program(SHELL, ruler_shell(&seen))
        .worktrees(worktrees.clone())
        .start();
    let net = h.network().clone();
    let connect: Connector<FakeLink> =
        Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string()));
    let clock = || Timestamp::parse("2026-10-29T12:00:00Z").unwrap();
    let mut client = ClientRuntime::new(
        MachineId::new("laptop"),
        connect,
        FakeHostLauncher::new(),
        clock,
    );
    client.set_cwd(app.to_str().unwrap());
    let mut s = Setup {
        _tmp: tmp,
        root,
        worktrees,
        h,
        client,
        seen,
    };
    s.client.start();
    s.settle(true);
    assert_eq!(s.client.client().connection(), &Connection::Ready);
    assert_eq!(s.h.host().workspace(), Some(app.as_path()));
    s.term(Event::Resize(WINDOW.0, WINDOW.1), true);
    s
}

/// The text of the pane's row `y` (relative to the pane) in `view`'s frame.
fn pane_row(view: &TerminalView<TestBackend>, y: u16) -> String {
    let buf = view.terminal().backend().buffer();
    let pane = layout(buf.area).pane;
    (pane.x..pane.right())
        .map(|x| buf[(x, pane.y + y)].symbol().to_owned())
        .collect()
}

#[test]
fn a_new_worktree_session_spawns_at_the_views_size_and_its_first_frame_needs_no_resize() {
    let mut s = setup();
    std::fs::create_dir_all(s.root.join("app").join("shell-1")).unwrap();

    s.client.user(Intent::NewSession);
    s.settle(true);

    let view_size = pane_size(WINDOW.0, WINDOW.1);
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.started, [view_size]);
    // The client's resize on selecting it, if it comes after the spawn, changes nothing.
    assert!(seen.resized.iter().all(|s| *s == view_size), "{seen:?}");
    drop(seen);
    let session = s.client.client().selected().cloned().unwrap();
    assert!(s.h.runtime().pty().is_running(&session));
    let view = s.frame();
    assert_eq!(pane_row(&view, 0), "=".repeat(usize::from(view_size.cols)));
    assert_eq!(pane_row(&view, 1).trim(), "");
}

#[test]
fn a_window_resized_while_the_worktree_is_made_spawns_the_session_at_the_new_size() {
    let mut s = setup();
    let hold = s.worktrees.hold_next();
    s.client.user(Intent::NewSession);
    s.settle(false);
    assert!(hold.wait_entered(LIMIT), "create never reached the port");
    let session: SessionId = s.client.client().selected().cloned().unwrap();
    assert!(!s.h.runtime().pty().is_running(&session));

    // The window changes while git works: the client resizes the session, which has no PTY yet.
    s.term(Event::Resize(WINDOW.0 - 10, WINDOW.1 - 5), false);
    s.term(Event::Resize(WINDOW.0, WINDOW.1), false);
    s.term(Event::Resize(WINDOW.0 - 20, WINDOW.1 - 8), false);
    std::fs::create_dir_all(s.root.join("app").join("shell-1")).unwrap();
    hold.release();
    s.settle(true);

    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.started, [pane_size(WINDOW.0 - 20, WINDOW.1 - 8)]);
    assert_eq!(seen.resized, []);
}
