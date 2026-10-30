//! The panic hook restores the terminal before the panic message is printed. Its own test binary, because a panic
//! hook is process-wide.

mod support {
    pub mod fake_terminal;
}

use std::sync::{Arc, Mutex, PoisonError};

use shepherd::terminal::{TerminalSetup, install_panic_hook, with_terminal};
use support::fake_terminal::FakeTerminal;

#[test]
fn a_panic_restores_the_terminal_before_the_previous_hook_reports_it() {
    let order: Arc<Mutex<Vec<String>>> = Arc::default();
    let terminal = FakeTerminal::new();
    {
        let order = order.clone();
        let terminal = terminal.clone();
        std::panic::set_hook(Box::new(move |_| {
            let calls = terminal.calls().join(",");
            order
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("previous hook saw [{calls}]"));
        }));
    }
    let setup: Arc<dyn TerminalSetup> = terminal.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_terminal(setup, |restorer| {
            install_panic_hook(restorer.clone());
            panic!("boom");
        })
    }));
    let _ = std::panic::take_hook();
    assert!(result.is_err());
    assert_eq!(
        *order.lock().unwrap_or_else(PoisonError::into_inner),
        ["previous hook saw [enter,leave]"]
    );
    // Unwinding past the guard doesn't leave a second time.
    assert_eq!(terminal.calls(), ["enter", "leave"]);
}
