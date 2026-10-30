//! A screen panic caught by `screen_guard` doesn't fire the client's panic hook, so the terminal stays set up and the
//! client keeps running, while a later real panic still restores the terminal and stops the client. Its own test
//! binary, because a panic hook is process-wide.

mod support {
    pub mod fake_terminal;
}

use std::sync::Arc;

use shepherd::client::{ClientRuntime, Connector, install_client_panic_hook};
use shepherd::screen_guard;
use shepherd::terminal::{TerminalSetup, with_terminal};
use shepherd_core::acpp::Timestamp;
use shepherd_core::ids::MachineId;
use shepherd_term::{PANIC_TRIGGER, Screen};
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use support::fake_terminal::FakeTerminal;

#[test]
fn a_caught_screen_panic_leaves_the_terminal_and_the_client_alone() {
    let terminal = FakeTerminal::enhanced();
    let net = FakeNetwork::new();
    let connect: Connector<FakeLink> =
        Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string()));
    let clock = || Timestamp::parse("2026-10-29T12:00:00Z").unwrap_or_else(|e| panic!("{e}"));
    let mut runtime = ClientRuntime::new(
        MachineId::new("m_client"),
        connect,
        FakeHostLauncher::new(),
        clock,
    );
    let setup: Arc<dyn TerminalSetup> = terminal.clone();

    let result = with_terminal(setup, |restorer| {
        install_client_panic_hook(restorer.clone(), runtime.events());
        let mut screen = Screen::new(30, 7);
        let caught = screen_guard::update(&mut screen, |s| s.feed(PANIC_TRIGGER));
        runtime.pump();
        let after_caught = (caught, screen.size(), terminal.calls(), runtime.done());
        // A real panic on the same thread afterwards still fires the hook.
        let real = std::panic::catch_unwind(|| panic!("a real panic"));
        assert!(real.is_err());
        runtime.pump();
        (after_caught, terminal.calls(), runtime.done())
    });
    let _ = std::panic::take_hook();

    let ((caught, size, calls, done), calls_after_real, done_after_real) =
        result.unwrap_or_else(|e| panic!("{e}"));
    assert!(
        caught.is_err_and(|why| why.contains("test panic trigger")),
        "the panic is caught and its message returned"
    );
    assert_eq!(size, (30, 7), "a blank screen of the same size");
    assert_eq!(calls, ["enter", "push keyboard", "mouse on"], "no restore");
    assert!(!done, "the client keeps running");
    assert_eq!(
        calls_after_real,
        [
            "enter",
            "push keyboard",
            "mouse on",
            "mouse off",
            "pop keyboard",
            "leave"
        ],
        "a later real panic restores the terminal"
    );
    assert!(done_after_real, "and stops the client");
}
