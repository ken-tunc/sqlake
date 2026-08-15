//! Argument parsing, dependency wiring, startup. Nothing else lives here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store};
use sqlake_config::Config;
use sqlake_core::id::ProfileId;
use sqlake_core::profile::Profiles;
use sqlake_driver_mock::{MockDriver, MockProfiles};
use sqlake_driver_postgres::PgDriver;
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

    /// Ignore the config file and open the built-in mock database.
    ///
    /// What the client ran against for the whole of M0. It is worth keeping:
    /// it is how every screen can be looked at without a server, and how a bug
    /// report can be reproduced by somebody with no access to the database it
    /// happened on.
    #[arg(long)]
    mock: bool,

    /// Connect to these profiles at startup instead of the first one.
    ///
    /// Names come from `connections.toml`. Several are allowed: two
    /// connections is the ordinary case, not an exotic one.
    #[arg(long = "connect", value_name = "PROFILE")]
    connect: Vec<String>,

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

    let profiles = profiles(&args)?;
    let opening = opening(&profiles, &args)?;

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let store = runtime.block_on(async {
        let store = Store::spawn(drivers(), profiles);
        for id in opening {
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

    // The user asked to quit, so the client goes.
    //
    // Dropping the runtime instead would wait for every blocking task, and
    // resolving a profile is one: it can be sitting on a keyring dialog that
    // nobody is going to answer, which would hang the exit behind a window the
    // user may not even be able to see. Nothing at this point needs to run —
    // when something does, it gets its own await *before* this line rather
    // than a longer timeout here.
    runtime.shutdown_timeout(Duration::from_millis(500));

    // The guard restores the screen as it drops, which happens on the way out
    // of this function whether `result` is an error or not.
    result.context("the render loop stopped")?;
    Ok(())
}

/// Every driver this build can talk to.
///
/// All of them, always: which one a connection needs is a fact about its
/// profile, and a registry that depended on the flags would make `--mock` mean
/// "and nothing else works".
fn drivers() -> Drivers {
    Drivers::new()
        .with(Arc::new(MockDriver::default()))
        .with(Arc::new(PgDriver::new()))
}

fn profiles(args: &Args) -> Result<Arc<dyn Profiles>> {
    if args.mock {
        return Ok(Arc::new(MockProfiles::default()));
    }
    // Reading it here rather than inside the store means a broken file is a
    // message on a terminal that still works, instead of an error raised into
    // a client that has already taken the screen over.
    let config = Config::load().context("reading the configuration")?;
    Ok(Arc::new(config))
}

/// Which profiles to open at startup.
///
/// The first one when nothing was asked for. Opening every profile would put a
/// keyring prompt and a connection attempt in front of somebody who wanted to
/// look at one database.
fn opening(profiles: &Arc<dyn Profiles>, args: &Args) -> Result<Vec<ProfileId>> {
    let available = profiles.list();
    if args.connect.is_empty() {
        return Ok(available
            .first()
            .map(|p| p.id.clone())
            .into_iter()
            .collect());
    }

    args.connect
        .iter()
        .map(|name| {
            let id = ProfileId::parse(name).map_err(|why| anyhow::anyhow!("--connect: {why}"))?;
            if available.iter().any(|p| p.id == id) {
                Ok(id)
            } else {
                // Before the screen is taken over, where a message can be read
                // — and with the list, because the usual cause is a typo.
                let names: Vec<&str> = available.iter().map(|p| p.id.as_str()).collect();
                Err(anyhow::anyhow!(
                    "--connect: no connection called `{name}`. Configured: {}",
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    }
                ))
            }
        })
        .collect()
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
