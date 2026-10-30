//! Setting up the user's terminal for the TUI, and putting it back however the TUI ends.
//!
//! [`TerminalSetup`] is the seam: [`CrosstermSetup`] does it for real (raw mode, the alternate screen, bracketed paste,
//! focus reports, kitty keyboard flags where the terminal supports them, and mouse capture), and tests record calls
//! instead. Mouse capture is on for the client's whole life, so the sidebar and the pane can be clicked (decision 0014);
//! the user selects text with the outer terminal's bypass (shift-drag, or option-drag in iTerm2 and Terminal.app).
//! [`TerminalGuard`] restores the terminal when dropped, so a normal return, an error and a panic that unwinds all
//! restore it, keyboard flags and mouse capture included. [`install_panic_hook`] restores
//! it before the panic message is printed, so the message lands on the normal screen where the user can read it, and
//! stops the client loop with an error, so a panic on another thread doesn't leave it drawing and `shepherd` exits
//! non-zero. Signals (SIGTERM, SIGHUP, SIGINT) quit the client loop, which then returns through the guard (see
//! [`crate::client::forward_signals`]).
//!
//! [`Restorer::alert`] tells the user a session wants them (the terminal bell and an OSC 9 notification), only while
//! the terminal is still ours.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::{cursor, execute};

/// What the TUI does to the user's terminal.
pub trait TerminalSetup: Send + Sync {
    /// Takes the terminal over. On failure, [`TerminalSetup::leave`] is still called, to undo what was done.
    fn enter(&self) -> io::Result<()>;
    /// After `enter`: asks the terminal to report keys unambiguously (kitty keyboard protocol: disambiguate escape
    /// codes and alternate keys), so Shift+Enter, ctrl-i against Tab, and Esc against alt reach us as themselves.
    /// Returns whether it did; a terminal without support is left alone.
    fn push_keyboard(&self) -> io::Result<bool>;
    /// Undoes a [`TerminalSetup::push_keyboard`] that returned true. Called before `leave`, while still on the
    /// alternate screen, whose flag stack the push went on.
    fn pop_keyboard(&self) -> io::Result<()>;
    /// Puts everything [`TerminalSetup::enter`] and [`TerminalSetup::set_mouse_capture`] changed back. Must be safe
    /// to call after a partial `enter`.
    fn leave(&self) -> io::Result<()>;
    /// Whether mouse events come to us or stay with the outer terminal. Turned on after `push_keyboard` and off
    /// before `pop_keyboard`.
    fn set_mouse_capture(&self, on: bool) -> io::Result<()>;
    /// Gets the user's attention with `message`: rings the bell and posts it as a notification where the terminal
    /// supports that. A terminal that can't does nothing.
    fn alert(&self, _message: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Restores the terminal once: later calls do nothing. Cheap to clone and share with a panic hook.
#[derive(Clone)]
pub struct Restorer {
    setup: Arc<dyn TerminalSetup>,
    active: Arc<AtomicBool>,
    /// Keyboard flags were pushed and not yet popped.
    keyboard: Arc<AtomicBool>,
    /// Mouse capture was turned on and not yet off.
    mouse: Arc<AtomicBool>,
}

impl Restorer {
    pub fn restore(&self) {
        if self.active.swap(false, Ordering::SeqCst) {
            // Nothing better to do if the terminal is gone (e.g. after SIGHUP).
            if self.mouse.swap(false, Ordering::SeqCst) {
                let _ = self.setup.set_mouse_capture(false);
            }
            if self.keyboard.swap(false, Ordering::SeqCst) {
                let _ = self.setup.pop_keyboard();
            }
            let _ = self.setup.leave();
        }
    }

    /// Alerts the user with `message` (see [`TerminalSetup::alert`]), unless the terminal was already restored.
    pub fn alert(&self, message: &str) {
        if self.active.load(Ordering::SeqCst) {
            // An alert that can't be shown is not worth stopping for.
            let _ = self.setup.alert(message);
        }
    }

    /// The terminal reports keys unambiguously (see [`TerminalSetup::push_keyboard`]).
    pub fn keyboard_enhanced(&self) -> bool {
        self.keyboard.load(Ordering::SeqCst)
    }
}

/// The terminal, taken over. Dropping it restores the terminal.
pub struct TerminalGuard {
    restorer: Restorer,
}

impl TerminalGuard {
    /// Takes the terminal over, mouse capture included. If that fails or panics partway, what was done is undone
    /// before returning the error or unwinding: the guard exists first, so its drop restores. A terminal that can't be
    /// asked for unambiguous keys is still used, with legacy keys.
    pub fn enter(setup: Arc<dyn TerminalSetup>) -> io::Result<Self> {
        let guard = Self {
            restorer: Restorer {
                setup,
                active: Arc::new(AtomicBool::new(true)),
                keyboard: Arc::new(AtomicBool::new(false)),
                mouse: Arc::new(AtomicBool::new(false)),
            },
        };
        let restorer = &guard.restorer;
        restorer.setup.enter()?;
        let pushed = restorer.setup.push_keyboard().unwrap_or(false);
        restorer.keyboard.store(pushed, Ordering::SeqCst);
        // A capture that fails partway is undone by `leave`.
        restorer.setup.set_mouse_capture(true)?;
        restorer.mouse.store(true, Ordering::SeqCst);
        Ok(guard)
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

/// At the start of any panic, on any thread: restores the terminal, runs the hook that was installed before (which
/// prints the message), then calls `on_panic` (the client stops its loop with an error, see
/// [`crate::client::install_client_panic_hook`]).
pub fn install_panic_hook(restorer: Restorer, on_panic: impl Fn() + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A screen panic is caught and the screen replaced; the terminal and the client carry on.
        if crate::screen_guard::guarding() {
            return;
        }
        restorer.restore();
        previous(info);
        on_panic();
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

    fn push_keyboard(&self) -> io::Result<bool> {
        if !supports_keyboard_enhancement()? {
            return Ok(false);
        }
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
        execute!(io::stdout(), PushKeyboardEnhancementFlags(flags))?;
        Ok(true)
    }

    fn pop_keyboard(&self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
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

    fn alert(&self, message: &str) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(&alert_sequence(message))?;
        out.flush()
    }
}

/// The bytes that alert the user with `message`: BEL, then OSC 9 (a desktop notification in iTerm2, kitty, WezTerm,
/// Ghostty and others; ignored elsewhere) ended by ST. Control characters in `message` are dropped, so it can't end
/// the sequence early.
pub fn alert_sequence(message: &str) -> Vec<u8> {
    let text: String = message.chars().filter(|c| !c.is_control()).collect();
    format!("\x07\x1b]9;{text}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alert_rings_the_bell_then_posts_the_message_with_osc_9() {
        assert_eq!(
            alert_sequence("api is idle"),
            b"\x07\x1b]9;api is idle\x1b\\"
        );
    }

    #[test]
    fn control_characters_cannot_end_the_alert_early() {
        assert_eq!(
            alert_sequence("a\x1b\\b\x07c\nd"),
            b"\x07\x1b]9;a\\bcd\x1b\\"
        );
    }

    #[test]
    fn c1_control_characters_cannot_end_the_alert_early() {
        // U+009C is ST and U+009B is CSI where a terminal reads C1 controls.
        assert_eq!(alert_sequence("a\u{9c}b\u{9b}c"), b"\x07\x1b]9;abc\x1b\\");
    }
}
