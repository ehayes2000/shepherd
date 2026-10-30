//! A panic in one session's emulator costs only that session's screen, and is logged to `host.log`. Alone in its
//! own test binary: tracing caches each callsite's interest process-wide, so another test hitting the same callsite
//! with no subscriber could make this test's thread-local subscriber miss it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::sync::{Arc, Mutex};

use shepherd::host;
use shepherd_term::PANIC_TRIGGER;
use shepherd_testkit::fakes::pty::{Program, ProgramInput};
use support::*;

#[test]
fn a_screen_that_panics_is_replaced_with_a_blank_one_of_the_same_size_and_logged() {
    let stdin = Arc::new(Mutex::new(Vec::new()));
    let record = stdin.clone();
    // Panics the host's screen once resized, then on `go` prints 100 columns and asks where the cursor is.
    let program: Program = Arc::new(move |input, p| match input {
        ProgramInput::Resized => p.print(PANIC_TRIGGER),
        ProgramInput::Stdin(b"go") => p.print(format!("{}\x1b[6n", "x".repeat(100))),
        ProgramInput::Stdin(bytes) => record.lock().unwrap().extend_from_slice(bytes),
        ProgramInput::Start => {}
    });
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("host.log");
    let file = fs::File::create(&log_path).unwrap();

    tracing::subscriber::with_default(host::file_subscriber(file), || {
        let mut h = HostHarness::builder().program(SHELL, program).start();
        let mut client = h.connect();
        client.subscribe_all();
        let req = client.new_session();
        h.step();
        let id = new_session_id(&client.take(), &req);
        client.resize(&id, 120, 40);
        h.step();
        client.input(&id, b"go");
        h.step();
    });

    // The fresh screen is 120 wide: at 80 columns the line would have wrapped.
    assert_eq!(stdin.lock().unwrap().as_slice(), b"\x1b[1;101R");
    let logged = fs::read_to_string(&log_path).unwrap();
    assert!(logged.contains("screen panicked"), "{logged}");
    assert!(logged.contains("test panic trigger"), "{logged}");
}
