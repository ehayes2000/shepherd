//! The terminal is restored on every way out of the TUI, checked through the `TerminalSetup` seam.

mod support {
    pub mod fake_terminal;
}

use std::sync::Arc;

use shepherd::terminal::{TerminalGuard, TerminalSetup, with_terminal};
use support::fake_terminal::FakeTerminal;

fn setup(terminal: &Arc<FakeTerminal>) -> Arc<dyn TerminalSetup> {
    terminal.clone()
}

#[test]
fn the_terminal_is_restored_after_a_normal_return() {
    let terminal = FakeTerminal::new();
    let out = with_terminal(setup(&terminal), |_| 7).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out, 7);
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn the_terminal_is_restored_after_an_error() {
    let terminal = FakeTerminal::new();
    let out = with_terminal(setup(&terminal), |_| Err::<(), _>("broken"));
    assert_eq!(out.ok(), Some(Err("broken")));
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn the_terminal_is_restored_when_the_body_panics() {
    let terminal = FakeTerminal::new();
    let s = setup(&terminal);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        with_terminal(s, |_| panic!("boom"))
    }));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn a_setup_that_fails_partway_is_undone() {
    let terminal = FakeTerminal::failing_enter();
    let result = TerminalGuard::enter(setup(&terminal));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

#[test]
fn restoring_early_leaves_only_once() {
    let terminal = FakeTerminal::new();
    with_terminal(setup(&terminal), |restorer| {
        restorer.restore();
        restorer.restore();
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}
