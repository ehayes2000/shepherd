//! A [`TerminalSetup`] that records what was done to the terminal instead of doing it.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use shepherd::terminal::TerminalSetup;

#[derive(Default)]
pub struct FakeTerminal {
    calls: Mutex<Vec<&'static str>>,
    fail_enter: bool,
    /// The outer terminal supports the kitty keyboard protocol.
    enhanced: bool,
}

// Not every test binary uses every constructor.
#[allow(dead_code)]
impl FakeTerminal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One that supports reporting keys unambiguously, so the setup pushes keyboard flags.
    pub fn enhanced() -> Arc<Self> {
        Arc::new(Self {
            enhanced: true,
            ..Self::default()
        })
    }

    /// One whose `enter` fails after recording itself, as a setup that fails partway would.
    pub fn failing_enter() -> Arc<Self> {
        Arc::new(Self {
            fail_enter: true,
            enhanced: true,
            ..Self::default()
        })
    }

    /// `enter`, `push keyboard`, `pop keyboard`, `leave`, `mouse on` and `mouse off`, in the order they happened.
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

    fn push_keyboard(&self) -> io::Result<bool> {
        if self.enhanced {
            self.record("push keyboard");
        }
        Ok(self.enhanced)
    }

    fn pop_keyboard(&self) -> io::Result<()> {
        self.record("pop keyboard");
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
