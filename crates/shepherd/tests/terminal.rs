//! The terminal is restored on every way out of the TUI, checked through the `TerminalSetup` seam. An outer terminal
//! that supports the kitty keyboard protocol has its keyboard flags pushed after setup and popped before the rest is
//! undone, on every path.

mod support {
    pub mod fake_terminal;
}

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use shepherd::terminal::{TerminalGuard, TerminalSetup, with_terminal};
use support::fake_terminal::FakeTerminal;

const RESTORED: [&str; 4] = ["enter", "push keyboard", "pop keyboard", "leave"];

fn setup(terminal: &Arc<FakeTerminal>) -> Arc<dyn TerminalSetup> {
    terminal.clone()
}

#[test]
fn the_terminal_is_restored_after_a_normal_return() {
    let terminal = FakeTerminal::enhanced();
    let out = with_terminal(setup(&terminal), |_| 7).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out, 7);
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn the_terminal_is_restored_after_an_error() {
    let terminal = FakeTerminal::enhanced();
    let out = with_terminal(setup(&terminal), |_| Err::<(), _>("broken"));
    assert_eq!(out.ok(), Some(Err("broken")));
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn the_terminal_is_restored_when_the_body_panics() {
    let terminal = FakeTerminal::enhanced();
    let s = setup(&terminal);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        with_terminal(s, |_| panic!("boom"))
    }));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn a_setup_that_fails_partway_is_undone() {
    let terminal = FakeTerminal::failing_enter();
    let result = TerminalGuard::enter(setup(&terminal));
    assert!(result.is_err());
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}

/// A terminal whose `enter` or `push_keyboard` panics after recording itself, as one that fails partway would.
struct PanickingTerminal {
    calls: Mutex<Vec<&'static str>>,
    panic_in: &'static str,
}

impl PanickingTerminal {
    fn new(panic_in: &'static str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            panic_in,
        })
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: &'static str) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
        assert_ne!(call, self.panic_in, "{call} panicked");
    }
}

impl TerminalSetup for PanickingTerminal {
    fn enter(&self) -> io::Result<()> {
        self.record("enter");
        Ok(())
    }

    fn push_keyboard(&self) -> io::Result<bool> {
        self.record("push keyboard");
        Ok(true)
    }

    fn pop_keyboard(&self) -> io::Result<()> {
        self.record("pop keyboard");
        Ok(())
    }

    fn leave(&self) -> io::Result<()> {
        self.record("leave");
        Ok(())
    }

    fn set_mouse_capture(&self, _: bool) -> io::Result<()> {
        Ok(())
    }
}

fn enter_panicking(terminal: &Arc<PanickingTerminal>) -> Vec<&'static str> {
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        with_terminal(setup, |_| ())
    }));
    assert!(result.is_err(), "the setup panicked");
    terminal.calls()
}

#[test]
fn a_setup_that_panics_partway_is_undone() {
    let terminal = PanickingTerminal::new("enter");
    assert_eq!(enter_panicking(&terminal), ["enter", "leave"]);
}

#[test]
fn a_keyboard_push_that_panics_is_undone_without_a_pop() {
    let terminal = PanickingTerminal::new("push keyboard");
    assert_eq!(
        enter_panicking(&terminal),
        ["enter", "push keyboard", "leave"]
    );
}

#[test]
fn restoring_early_leaves_only_once() {
    let terminal = FakeTerminal::enhanced();
    with_terminal(setup(&terminal), |restorer| {
        restorer.restore();
        restorer.restore();
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(terminal.calls(), RESTORED);
}

#[test]
fn keyboard_flags_are_pushed_and_popped_only_where_supported() {
    let terminal = FakeTerminal::new();
    let enhanced = with_terminal(setup(&terminal), |r| r.keyboard_enhanced())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!enhanced);
    assert_eq!(terminal.calls(), ["enter", "leave"]);

    let terminal = FakeTerminal::enhanced();
    let enhanced = with_terminal(setup(&terminal), |r| r.keyboard_enhanced())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(enhanced);
}
