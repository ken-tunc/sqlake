//! Argument parsing, dependency wiring, startup. Nothing else lives here.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store};
use sqlake_core::capability::DriverKind;
use sqlake_driver_mock::MockDriver;
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
    #[arg(long, default_value = "info")]
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
    install_panic_hook(!args.no_mouse);

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let store = runtime.block_on(async {
        let store = Store::spawn(Drivers::new().with(std::sync::Arc::new(MockDriver::default())));
        // M0 has one driver and no profiles, so the connection the user would
        // have chosen is the only one there is. `sqlake-config` replaces this
        // in M1.
        store.dispatch(Action::Connect(DriverKind::Mock));
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

    let file = tracing_appender::rolling::never(&dir, "sqlake.log");
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

fn log_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("sqlake")
}
