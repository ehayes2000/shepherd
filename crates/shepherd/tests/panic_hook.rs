//! The panic hook restores the terminal before the panic message is printed, and stops the client loop with an error,
//! so a panic on another thread doesn't leave the loop drawing on a terminal that is no longer set up, and `shepherd`
//! exits non-zero. Its own test binary, because a panic hook is process-wide.

mod support {
    pub mod fake_terminal;
}

use std::sync::{Arc, Mutex, PoisonError};

use shepherd::client::{ClientError, ClientRuntime, Connector, install_client_panic_hook};
use shepherd::terminal::{TerminalSetup, with_terminal};
use shepherd_core::acpp::Timestamp;
use shepherd_core::ids::MachineId;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use support::fake_terminal::FakeTerminal;

#[test]
fn a_panic_on_another_thread_restores_the_terminal_first_and_fails_the_client() {
    let order: Arc<Mutex<Vec<String>>> = Arc::default();
    let terminal = FakeTerminal::enhanced();
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
    // A client that never reaches a host, so only the panic can stop it.
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
        let worker = std::thread::spawn(|| panic!("boom"));
        assert!(worker.join().is_err());
        runtime.run()
    });
    let _ = std::panic::take_hook();

    assert_eq!(
        result.ok(),
        Some(Err(ClientError::Panicked)),
        "the client stopped with an error"
    );
    assert_eq!(
        *order.lock().unwrap_or_else(PoisonError::into_inner),
        ["previous hook saw [enter,push keyboard,pop keyboard,leave]"]
    );
    // Returning through the guard doesn't restore a second time.
    assert_eq!(
        terminal.calls(),
        ["enter", "push keyboard", "pop keyboard", "leave"]
    );
}
