//! Runs a session's [`Screen`] so a panic in the emulator costs only that screen, not the host or the client.
//!
//! Program output reaches alacritty unfiltered, and alacritty has panicked on it before (F23). The host and client
//! runtimes feed screens through [`update`]: on a panic the screen is replaced with a blank one of the same size and
//! they carry on. The client's panic hook skips panics caught here ([`guarding`]), so it doesn't restore the terminal
//! and quit.

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use shepherd_term::Screen;

thread_local! {
    static GUARDING: Cell<bool> = const { Cell::new(false) };
}

/// Runs `update` on `screen`. If it panics, `screen` is replaced with a blank screen of the same size and the panic
/// message is returned.
pub fn update(screen: &mut Screen, update: impl FnOnce(&mut Screen)) -> Result<(), String> {
    let (cols, rows) = screen.size();
    let was = GUARDING.replace(true);
    let result = catch_unwind(AssertUnwindSafe(|| update(screen)));
    GUARDING.set(was);
    result.map_err(|payload| {
        *screen = Screen::new(cols, rows);
        message(payload.as_ref())
    })
}

/// Whether this thread is inside [`update`], where a panic is caught and handled.
pub fn guarding() -> bool {
    GUARDING.get()
}

fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic with no message".to_owned()
    }
}
