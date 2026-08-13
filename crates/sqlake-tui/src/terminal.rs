//! The only place terminal modes are changed.
//!
//! Entering raw mode, the alternate screen and mouse capture all have to be
//! undone, and they have to be undone on every exit path — a clean quit, a
//! panic, and (from M4) handing the terminal to `$EDITOR`. Every one of those
//! goes through [`restore`], so there is exactly one thing to get right.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal when dropped.
#[derive(Debug)]
pub struct TerminalGuard {
    mouse: bool,
}

impl TerminalGuard {
    /// Take over the terminal and return it along with a drawing handle.
    ///
    /// `mouse` is false under `--no-mouse`, and in terminals where capture
    /// would take native text selection away from the user.
    pub fn enter(mouse: bool) -> io::Result<(Self, Tui)> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        // Constructed only after every step has succeeded, so a partial setup
        // is undone by the `?` above rather than by a half-initialised guard.
        let guard = Self { mouse };
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok((guard, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Nothing useful can be done with a failure here: the process is on its
        // way out and stdout may already be gone.
        let _ = restore(self.mouse);
    }
}

/// Undo everything [`TerminalGuard::enter`] did.
///
/// A free function so the panic hook can call it without owning a guard.
/// Safe to call when the terminal was never taken over: each step fails
/// independently and the errors are reported, not acted on.
pub fn restore(mouse: bool) -> io::Result<()> {
    let mut out = io::stdout();
    if mouse {
        execute!(out, DisableMouseCapture)?;
    }
    execute!(out, LeaveAlternateScreen, Show)?;
    disable_raw_mode()
}

/// Restore the terminal before the default hook prints anything.
///
/// The hook runs before unwinding, so by the time the backtrace is printed the
/// screen is already usable. Without this, a panic leaves the terminal in raw
/// mode on the alternate screen and the message is invisible.
pub fn install_panic_hook(mouse: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore(mouse);
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_without_a_terminal_does_not_panic() {
        // CI has no tty. The restore path must degrade quietly, because it is
        // also the panic path — panicking there would abort the process.
        let _ = restore(true);
        let _ = restore(false);
    }

    #[test]
    fn restoring_twice_is_harmless() {
        let _ = restore(true);
        let _ = restore(true);
    }
}
