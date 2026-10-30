//! A [`TerminalSetup`] that records what was done to the terminal instead of doing it.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use shepherd::terminal::TerminalSetup;

#[derive(Default)]
pub struct FakeTerminal {
    calls: Mutex<Vec<&'static str>>,
    fail_enter: bool,
}

impl FakeTerminal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One whose `enter` fails after recording itself, as a setup that fails partway would.
    #[allow(dead_code)] // Not every test binary uses it.
    pub fn failing_enter() -> Arc<Self> {
        Arc::new(Self {
            fail_enter: true,
            ..Self::default()
        })
    }

    /// `enter`, `leave`, `mouse on` and `mouse off`, in the order they happened.
    pub fn calls(&self) -> Vec<&'static str> {
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
    }
}

impl TerminalSetup for FakeTerminal {
    fn enter(&self) -> io::Result<()> {
        self.record("enter");
        if self.fail_enter {
            return Err(io::Error::other("no tty"));
        }
        Ok(())
    }

    fn leave(&self) -> io::Result<()> {
        self.record("leave");
        Ok(())
    }

    fn set_mouse_capture(&self, on: bool) -> io::Result<()> {
        self.record(if on { "mouse on" } else { "mouse off" });
        Ok(())
    }
}
