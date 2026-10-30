//! Setting up the user's terminal for the TUI, and putting it back however the TUI ends.
//!
//! [`TerminalSetup`] is the seam: [`CrosstermSetup`] does it for real (raw mode, the alternate screen, bracketed paste,
//! focus reports, and mouse capture while the selected session wants the mouse), and tests record calls instead.
//! [`TerminalGuard`] restores the terminal when dropped, so a normal return, an error and a panic that unwinds all
//! restore it. [`install_panic_hook`] restores it before the panic message is printed, so the message lands on the
//! normal screen where the user can read it. Signals (SIGTERM, SIGHUP, SIGINT) quit the client loop, which then returns
//! through the guard (see [`crate::client::forward_signals`]).

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

/// What the TUI does to the user's terminal.
pub trait TerminalSetup: Send + Sync {
    /// Takes the terminal over. On failure, [`TerminalSetup::leave`] is still called, to undo what was done.
    fn enter(&self) -> io::Result<()>;
    /// Puts everything [`TerminalSetup::enter`] and [`TerminalSetup::set_mouse_capture`] changed back. Must be safe
    /// to call after a partial `enter`.
    fn leave(&self) -> io::Result<()>;
    /// Whether mouse events come to us (and so to the selected session) or stay with the outer terminal, for
    /// selecting text.
    fn set_mouse_capture(&self, on: bool) -> io::Result<()>;
}

/// Restores the terminal once: later calls do nothing. Cheap to clone and share with a panic hook.
#[derive(Clone)]
pub struct Restorer {
    setup: Arc<dyn TerminalSetup>,
    active: Arc<AtomicBool>,
}

impl Restorer {
    pub fn restore(&self) {
        if self.active.swap(false, Ordering::SeqCst) {
            // Nothing better to do if the terminal is gone (e.g. after SIGHUP).
            let _ = self.setup.leave();
        }
    }
}

/// The terminal, taken over. Dropping it restores the terminal.
pub struct TerminalGuard {
    restorer: Restorer,
}

impl TerminalGuard {
    /// Takes the terminal over. If that fails partway, what was done is undone before returning the error.
    pub fn enter(setup: Arc<dyn TerminalSetup>) -> io::Result<Self> {
        let restorer = Restorer {
            setup,
            active: Arc::new(AtomicBool::new(true)),
        };
        if let Err(e) = restorer.setup.enter() {
            restorer.restore();
            return Err(e);
        }
        Ok(Self { restorer })
    }

    pub fn restorer(&self) -> &Restorer {
        &self.restorer
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restorer.restore();
    }
}

/// Runs `body` with the terminal taken over, and restores it however `body` ends: a return, an error it returns,
/// or a panic.
pub fn with_terminal<R>(
    setup: Arc<dyn TerminalSetup>,
    body: impl FnOnce(&Restorer) -> R,
) -> io::Result<R> {
    let guard = TerminalGuard::enter(setup)?;
    Ok(body(guard.restorer()))
}

/// Restores the terminal at the start of any panic, then runs the hook that was installed before.
pub fn install_panic_hook(restorer: Restorer) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restorer.restore();
        previous(info);
    }));
}

/// The real terminal, through crossterm, on stdout.
pub struct CrosstermSetup;

impl TerminalSetup for CrosstermSetup {
    fn enter(&self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableFocusChange
        )
    }

    fn leave(&self) -> io::Result<()> {
        // Every step runs even if one fails; the first error is reported.
        let mut out = io::stdout();
        let steps = [
            execute!(out, DisableMouseCapture),
            execute!(out, DisableFocusChange),
            execute!(out, DisableBracketedPaste),
            execute!(out, LeaveAlternateScreen),
            execute!(out, cursor::Show),
            disable_raw_mode(),
            out.flush(),
        ];
        steps.into_iter().collect()
    }

    fn set_mouse_capture(&self, on: bool) -> io::Result<()> {
        if on {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        }
    }
}
