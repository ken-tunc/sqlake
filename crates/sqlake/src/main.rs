//! Argument parsing, dependency wiring, startup. Nothing else lives here.

mod agent;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store};
use sqlake_config::{Config, Settings};
use sqlake_core::id::{ConnId, ProfileId};
use sqlake_core::profile::Profiles;
use sqlake_driver_bigquery::BqDriver;
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
        global = true,
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
    #[arg(long, global = true)]
    mock: bool,

    /// Connect to these profiles at startup instead of the first one.
    ///
    /// Names come from `connections.toml`. Several are allowed: two
    /// connections is the ordinary case, not an exotic one. A subcommand takes
    /// one, since it answers one request.
    #[arg(long = "connect", global = true, value_name = "PROFILE")]
    connect: Vec<String>,

    /// Panic once the terminal is taken over, to prove it is given back.
    ///
    /// The screen has to be restored from the panic hook rather than from a
    /// tidy exit path, and the only honest way to check that is to panic.
    #[arg(long, hide = true)]
    panic_test: bool,

    /// Listen on a named socket, so an agent can drive this session.
    ///
    /// Opt-in. A socket is a way into a process holding live connections and
    /// resolved credentials, so it is opened because somebody asked for it and
    /// not because the client started. `$SQLAKE_SESSION` names one too, which
    /// is what makes per-project addressing a shell convention rather than a
    /// feature this has to know about.
    #[arg(long, value_name = "NAME", global = true)]
    session: Option<String>,

    /// Answer one request and exit, instead of opening the client.
    ///
    /// The agent surface's one-shot mode. Every other flag still applies:
    /// `--mock` picks the built-in database, `--connect` picks a profile.
    #[command(subcommand)]
    command: Option<agent::Command>,
}

fn main() -> Result<std::process::ExitCode> {
    let args = Args::parse();
    let _log = init_logging(&args.log_level)?;

    // Before the panic hook and the terminal guard: a subcommand never takes
    // the screen, and installing a hook that restores a terminal nobody entered
    // would leave a stray reset in the middle of a caller's JSON.
    if let Some(command) = &args.command {
        let (profiles, page_size, connect) = if command.needs() == agent::Needs::Nothing {
            (
                Arc::new(agent::NoProfiles) as Arc<dyn Profiles>,
                Settings::default().page_size,
                None,
            )
        } else {
            let (profiles, settings) = configuration(&args)?;
            let named = opening(&profiles, &args)?;
            // One request, one connection. A second `--connect` is a caller
            // expecting an answer that covers both, and silently dropping it
            // answers about the first as though it had asked for only that.
            anyhow::ensure!(
                named.len() <= 1,
                "--connect: a subcommand opens one connection, and {} were named",
                named.len()
            );
            // Only a profile the caller actually named is a choice. `opening`
            // falls back to the first configured profile, which one-shot would
            // have picked anyway — but passing it on as a choice would make an
            // attached command demand *that* profile of a session that has a
            // different database open, and fail against a session it could
            // have read through.
            let connect = if args.connect.is_empty() {
                None
            } else {
                named.into_iter().next()
            };
            (profiles, settings.page_size, connect)
        };
        // Resolved even for a command that starts its own store: attaching is
        // tried first, and a session that is running is always the better
        // answer than a second store opening the same database again.
        let session =
            match sqlake_api::socket_path(&sqlake_api::session_name(args.session.as_deref())) {
                Ok(path) => Some(path),
                // Not fatal: a path that cannot name a socket means no session can
                // be reached, and the command runs its own store. Said out loud on
                // stderr, because a caller that named a session expected to attach
                // and would otherwise see a slower answer and no reason — and said
                // only then, since a caller that named none was never attaching.
                Err(why) => {
                    if args.session.is_some() || std::env::var_os("SQLAKE_SESSION").is_some() {
                        eprintln!("not attaching: {why}");
                    }
                    None
                }
            };
        return agent::run(command, drivers(), profiles, page_size, connect, session);
    }

    // Every mode change is undone by the guard's `Drop`, and the hook routes a
    // panic through the same function. Installed before the guard exists so a
    // failure inside `enter` is already covered.
    //
    // The hook ends the process rather than returning. tokio catches a panic in
    // a spawned task, so without this the screen would be restored while the
    // render loop carried on drawing frames over the user's shell, reading
    // input that the terminal is now echoing.
    install_panic_hook(!args.no_mouse);

    let (profiles, settings) = configuration(&args)?;
    let opening = opening(&profiles, &args)?;

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let store = runtime.block_on(async {
        let store = Store::spawn(
            drivers(),
            profiles,
            settings.page_size,
            settings.max_bytes_billed,
        );
        for profile in opening {
            store.dispatch(Action::Connect {
                profile,
                conn: ConnId::new(),
            });
        }
        store
    });

    // Opt-in, and bound before the screen is taken: a name already in use is a
    // message on a terminal somebody can read, rather than an error raised into
    // a client that has taken the display over. The listener is held until the
    // client exits, and removes its socket on the way out.
    let _listening = match &args.session {
        Some(name) => Some(listen(&runtime, name, &store)?),
        None => None,
    };

    // Before the screen is taken over: it reads the environment, and doing it
    // per keystroke would let a variable changed in another shell take effect
    // halfway through a session, with the file moving under it.
    let editor = editor(&settings);

    let (mut _guard, mut terminal) = TerminalGuard::enter(!args.no_mouse)?;
    assert!(
        !args.panic_test,
        "--panic-test: the terminal should come back"
    );

    let result = runtime.block_on(sqlake_tui::run(
        &mut terminal,
        &mut _guard,
        &store,
        !args.no_mouse,
        &editor,
    ));

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
    Ok(std::process::ExitCode::SUCCESS)
}

/// Answer the agent surface on this session's socket, alongside the client.
///
/// The same store the person is using, not a second one: reusing their
/// connections — and the credential prompts they have already answered — is
/// the whole reason to attach rather than start a store of one's own.
fn listen(
    runtime: &tokio::runtime::Runtime,
    name: &str,
    store: &Store,
) -> Result<sqlake_api::ListenerHandle> {
    let path = sqlake_api::socket_path(name).context("finding somewhere to put the socket")?;
    let listener = runtime
        .block_on(sqlake_api::Listener::bind(path.clone()))
        .with_context(|| format!("listening on {}", path.display()))?;
    tracing::info!(session = name, path = %path.display(), "answering the agent surface");
    Ok(listener.spawn_on(runtime, Arc::new(sqlake_api::Service::new(store.clone()))))
}

/// What `e` hands the buffer to, and where the working files go.
///
/// Resolved once, here, rather than per keystroke: a variable changed in
/// another shell must not take effect halfway through a session, and the file
/// would move with it.
fn editor(settings: &Settings) -> sqlake_tui::editor::Editor {
    // A temporary directory when there is nowhere else, for the reason
    // `log_dir` gives: no `$HOME` and no `$XDG_STATE_HOME` is not a reason to
    // refuse to start, and here it would be a refusal over a feature this run
    // may never use. The file only has to outlive the editor.
    let scratch = sqlake_config::paths::scratch_dir(
        &sqlake_config::paths::state_dir().unwrap_or_else(|_| std::env::temp_dir().join("sqlake")),
    );
    sqlake_tui::editor::Editor::new(
        settings.editor_program(std::env::var_os("VISUAL"), std::env::var_os("EDITOR")),
        settings.editor_args.clone(),
        scratch,
    )
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
        .with(Arc::new(BqDriver::new()))
}

fn configuration(args: &Args) -> Result<(Arc<dyn Profiles>, Settings)> {
    if args.mock {
        return Ok((Arc::new(MockProfiles::default()), Settings::default()));
    }
    // Reading it here rather than inside the store means a broken file is a
    // message on a terminal that still works, instead of an error raised into
    // a client that has already taken the screen over.
    let config = Config::load().context("reading the configuration")?;
    let settings = config.settings.clone();
    Ok((Arc::new(config), settings))
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
    // A bare level, not one directive per crate: the list of crates that log
    // is not this function's to keep in sync, and naming only `sqlake` and
    // `sqlake_app` once left `sqlake_driver_postgres`'s only warning — the one
    // on a connection ending — writing to a file nobody without `RUST_LOG` set
    // would ever see it in.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

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
