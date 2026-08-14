//! Argument parsing, dependency wiring, startup. Nothing else lives here.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store};
use sqlake_core::profile::Profiles as _;
use sqlake_driver_mock::{MockDriver, MockProfiles};
use sqlake_tui::terminal::{TerminalGuard, install_panic_hook};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "sqlake",
    about = "A mouse-friendly database client for the terminal"
)]
struct Args {
    /// Leave the mouse to the terminal.
    ///
    /// Capture takes native text selection away, and some terminals and tmux
    /// configurations cannot deliver the events anyway. Every operation has a
    /// key binding, so this costs nothing but convenience.
    #[arg(long)]
    no_mouse: bool,

    /// `error`, `warn`, `info`, `debug` or `trace`. `RUST_LOG` overrides it.
    ///
    /// Checked here rather than by the filter: `EnvFilter` accepts an unknown
    /// level, complains on stderr and falls back to errors only, so a typo
    /// would leave the log silently almost empty.
    #[arg(
        long,
        default_value = "info",
        value_parser = ["error", "warn", "info", "debug", "trace"]
    )]
    log_level: String,

    /// Panic once the terminal is taken over, to prove it is given back.
    ///
    /// The screen has to be restored from the panic hook rather than from a
    /// tidy exit path, and the only honest way to check that is to panic.
    #[arg(long, hide = true)]
    panic_test: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _log = init_logging(&args.log_level)?;

    // Every mode change is undone by the guard's `Drop`, and the hook routes a
    // panic through the same function. Installed before the guard exists so a
    // failure inside `enter` is already covered.
    //
    // The hook ends the process rather than returning. tokio catches a panic in
    // a spawned task, so without this the screen would be restored while the
    // render loop carried on drawing frames over the user's shell, reading
    // input that the terminal is now echoing.
    install_panic_hook(!args.no_mouse);

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let store = runtime.block_on(async {
        // Still the mock, and still the only profile there is. What changed is
        // that it arrives as a profile rather than as a driver kind, so the
        // path it takes is the path a real connection will take; T7 swaps this
        // for the profiles in `connections.toml`.
        let profiles = MockProfiles::default();
        let first = profiles.list().first().map(|p| p.id.clone());
        let store = Store::spawn(
            Drivers::new().with(std::sync::Arc::new(MockDriver::default())),
            std::sync::Arc::new(profiles),
        );
        if let Some(id) = first {
            store.dispatch(Action::Connect(id));
        }
        store
    });

    let (_guard, mut terminal) = TerminalGuard::enter(!args.no_mouse)?;
    assert!(
        !args.panic_test,
        "--panic-test: the terminal should come back"
    );

    let result = runtime.block_on(sqlake_tui::run(&mut terminal, &store, !args.no_mouse));

    // The guard restores the screen as it drops, which happens on the way out
    // of this function whether `result` is an error or not.
    result.context("the render loop stopped")?;
    Ok(())
}

/// Logs go to a file and nowhere else.
///
/// A single line on stdout while the alternate screen is up corrupts it, and
/// the corruption looks like a rendering bug rather than a stray `println!`.
fn init_logging(level: &str) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the log directory {}", dir.display()))?;

    // `rolling::never` panics inside itself when the file cannot be opened —
    // a read-only directory is enough — and a backtrace is a poor answer to
    // "the log path is not writable".
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::NEVER)
        .filename_suffix("sqlake.log")
        .build(&dir)
        .with_context(|| format!("opening the log file in {}", dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("sqlake={level},sqlake_app={level}")));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();

    tracing::info!(dir = %dir.display(), "logging to file");
    Ok(guard)
}

/// The state directory, or a temporary one.
///
/// Losing the log is better than refusing to start over it, which is what
/// happens in the one case `sqlake-config` cannot answer: no `$HOME` and no
/// `$XDG_STATE_HOME` at all.
fn log_dir() -> PathBuf {
    sqlake_config::paths::state_dir().unwrap_or_else(|_| std::env::temp_dir().join("sqlake"))
}
