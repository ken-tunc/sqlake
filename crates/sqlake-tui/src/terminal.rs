//! The only place terminal modes are changed.
//!
//! Entering raw mode, the alternate screen and mouse capture all have to be
//! undone, and they have to be undone on every exit path — a clean quit, a
//! panic, and (from M4) handing the terminal to `$EDITOR`. Every one of those
//! goes through [`restore`], so there is exactly one thing to get right.
//!
//! [`TerminalGuard::suspended`] has no test of its own, deliberately: it
//! enables raw mode on the real terminal, and a test that ran it would put the
//! terminal running the tests into raw mode. What it is made of is covered
//! instead — `restore` here, and `editor::edit` in its own module — leaving
//! only the order of the two, which is what `--panic-test` proves for the
//! sibling path through the same function.
//!
//! SIGTERM is the gap: a signal does not unwind, so `Drop` never runs and the
//! terminal is left in raw mode on the alternate screen. Closing it means a
//! signal handler that calls [`restore`], which nothing needs yet.

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

#[derive(Debug)]
pub struct TerminalGuard {
    mouse: bool,
}

impl TerminalGuard {
    /// `mouse` is false under `--no-mouse`, and in terminals where capture
    /// would take native text selection away from the user.
    pub fn enter(mouse: bool) -> io::Result<(Self, Tui)> {
        enable_raw_mode()?;
        // The guard exists from the first mode change onwards, so every `?`
        // below returns through its `Drop`. Constructing it only after the last
        // step would leave the terminal in raw mode on the alternate screen
        // whenever one of them failed.
        let mut guard = Self { mouse: false };
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, Hide)?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
            guard.mouse = true;
        }
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok((guard, terminal))
    }

    /// Give the terminal back for the duration of `f`, then take it again.
    ///
    /// The `$EDITOR` handoff, and the reason `restore` is a free function
    /// rather than only a `Drop`: the editor needs raw mode off, the main
    /// screen back and the cursor visible, which is precisely what quitting
    /// does — and doing it any other way would be a second teardown to keep
    /// in step with this one.
    ///
    /// The screen is redrawn from scratch on the way back in. The editor wrote
    /// over the alternate screen's contents, and ratatui's buffer still
    /// believes what it drew last, so without a clear the next frame updates
    /// only the cells it thinks changed and leaves the editor's output around
    /// them.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses. `f` has already run when a re-entry
    /// error is returned, so its effect is not undone — the caller is expected
    /// to stop drawing, which is what leaves `Drop` to tidy up.
    pub fn suspended<T>(&mut self, terminal: &mut Tui, f: impl FnOnce() -> T) -> io::Result<T> {
        let released = restore(self.mouse);
        let wanted_mouse = self.mouse;
        // Recorded before `f` runs: if re-entry fails half way, `Drop` must not
        // try to disable a capture that is not on.
        self.mouse = false;
        // Before `f`, not after: an editor started on a terminal still in raw
        // mode on the alternate screen is one nobody can type into, and the
        // client is on its way out either way.
        released?;
        let out = f();

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        if wanted_mouse {
            execute!(stdout, EnableMouseCapture)?;
            self.mouse = true;
        }
        terminal.clear()?;
        Ok(out)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Nothing useful can be done with a failure here: the process is on its
        // way out and stdout may already be gone.
        let _ = restore(self.mouse);
    }
}

/// A free function so the panic hook can call it without owning a guard.
/// Safe to call when the terminal was never taken over: each step fails
/// independently and the errors are reported, not acted on.
pub fn restore(mouse: bool) -> io::Result<()> {
    let mut out = io::stdout();
    // Every step runs even when an earlier one failed, and the first error is
    // returned afterwards. Short-circuiting on `?` here is how a terminal ends
    // up left in raw mode: the one write that failed would take the rest of the
    // teardown with it, on the path that runs while the process is panicking.
    let capture = if mouse {
        execute!(out, DisableMouseCapture)
    } else {
        Ok(())
    };
    let screen = execute!(out, LeaveAlternateScreen, Show);
    let raw = disable_raw_mode();
    capture.and(screen).and(raw)
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
        // The process ends here rather than unwinding.
        //
        // A panic in a spawned task is caught by tokio, so returning would
        // leave the render loop drawing frames over the shell this hook just
        // restored, reading input the terminal is now echoing. Anything that
        // panicked has also left the state it was editing half-written, and
        // there is no path back to a screen worth trusting.
        std::process::exit(101);
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
