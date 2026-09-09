//! The subcommands, and the one-shot run behind them.
//!
//! One-shot starts a store, opens one connection, answers one request and tears
//! the store down. It is the mode that works in CI and in a fresh shell, and it
//! is the simpler of the two, so it is the one built first — the socket reuses
//! the same [`Service`] against a store somebody else started.
//!
//! JSON goes to stdout and nothing else does. Diagnostics go to stderr, so a
//! caller can pipe stdout into a parser without filtering prose out of it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::{Args as ClapArgs, Subcommand};
use sqlake_api::{Failure, QueryState, Request, Response, Service, Status};
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store, Wiring};
use sqlake_config::Settings;
use sqlake_core::id::{ConnId, ProfileId};
use sqlake_core::profile::{ProfileError, ProfileSummary, Profiles, ResolvedProfile};

/// What a caller can ask for from the command line.
///
/// Noun then verb, and one subcommand per [`Request`]. The mapping is
/// deliberately dull: a subcommand that did more than name a request would be
/// behaviour the socket does not have.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// The surface itself.
    Api {
        #[command(subcommand)]
        what: ApiCommand,
    },
    /// Connections open in this session.
    Connection {
        #[command(subcommand)]
        what: ConnectionCommand,
    },
    /// Namespaces — what a driver calls a schema or a dataset.
    Schema {
        #[command(subcommand)]
        what: SchemaCommand,
    },
    /// Relations, and their contents.
    Table {
        #[command(subcommand)]
        what: TableCommand,
    },
    /// Statements, and what running them produced.
    Query {
        #[command(subcommand)]
        what: QueryCommand,
    },
    /// Speak MCP on stdin and stdout, exposing the same operations as tools.
    ///
    /// After the subcommands rather than instead of them: the CLI is testable
    /// with a shell and usable by anything that can run a command, and this is
    /// implemented in terms of it. Building MCP first would have meant
    /// debugging two layers at once.
    Mcp,
}

#[derive(Debug, Subcommand)]
pub(crate) enum QueryCommand {
    /// What a statement would cost, without running it.
    Estimate {
        #[arg(value_name = "SQL")]
        sql: String,
    },
    /// Run a statement. Answers with a handle rather than with rows.
    Run {
        #[arg(value_name = "SQL")]
        sql: String,
        /// A tighter byte ceiling than the session's. It cannot raise it.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<u64>,
    },
    /// Where a query has got to, answered at once.
    Status {
        #[arg(value_name = "ID")]
        query: String,
        /// Fewer rows than the budget allows. It cannot ask for more.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Block until a query finishes, fails, or the wait runs out.
    Wait {
        #[arg(value_name = "ID")]
        query: String,
        /// How long to wait. The session's own timeout is the ceiling.
        #[arg(long, value_name = "MS")]
        timeout_ms: Option<u64>,
        /// Fewer rows than the budget allows. It cannot ask for more.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Stop a query this session started.
    Cancel {
        #[arg(value_name = "ID")]
        query: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ApiCommand {
    /// The session's connections and the profiles it could open.
    Snapshot,
    /// The request and response schema, generated from the protocol types.
    Schema,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConnectionCommand {
    List,
    /// Open a connection to a configured profile, in a running session.
    Open {
        /// The profile's id, as written in `connections.toml`.
        #[arg(value_name = "PROFILE")]
        profile: String,
    },
    /// Close one of the session's connections.
    ///
    /// Which one is chosen the way every other command chooses: the session's
    /// ready connection, narrowed by `--connect`.
    Close,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SchemaCommand {
    List,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TableCommand {
    /// The relations in one namespace.
    List {
        #[command(flatten)]
        path: Path,
    },
    /// What a relation is: its columns, its indexes, and a generated
    /// `CREATE` statement.
    Describe {
        #[command(flatten)]
        path: Path,
        /// Ask the driver again rather than reading what the session holds.
        #[arg(long)]
        refresh: bool,
    },
    /// A page of one relation.
    Preview {
        #[command(flatten)]
        path: Path,
        /// Sort by a column, by its index in the page's `columns`.
        #[arg(long)]
        sort: Option<usize>,
        /// Fewer rows than the budget allows. It cannot ask for more.
        #[arg(long)]
        limit: Option<usize>,
    },
}

/// A path through the object tree.
///
/// Dotted, because `public.users` is what anybody types. A name that itself
/// contains a dot is a real thing, though — the protocol carries whole paths
/// rather than a joined name for exactly that reason — so `--part` gives a
/// segment verbatim and is what to reach for when splitting would be wrong.
#[derive(Debug, ClapArgs)]
pub(crate) struct Path {
    /// `public.users`, or `public` for a namespace.
    #[arg(value_name = "PATH", required_unless_present = "part")]
    dotted: Option<String>,

    /// One segment, taken as written. Repeat it for a deeper path.
    #[arg(long = "part", value_name = "SEGMENT", conflicts_with = "dotted")]
    part: Vec<String>,
}

impl Path {
    fn segments(&self) -> Vec<String> {
        if self.part.is_empty() {
            self.dotted
                .iter()
                .flat_map(|d| d.split('.'))
                .map(str::to_owned)
                .collect()
        } else {
            self.part.clone()
        }
    }
}

impl Command {
    /// The request this subcommand names, against a connection the caller has
    /// already opened.
    fn request(&self, connection: String) -> Request {
        match self {
            Self::Api {
                what: ApiCommand::Snapshot,
            } => Request::Snapshot {},
            Self::Api {
                what: ApiCommand::Schema,
            } => Request::Schema {},
            Self::Connection {
                what: ConnectionCommand::List,
            } => Request::ConnectionList {},
            Self::Connection {
                what: ConnectionCommand::Open { profile },
            } => Request::ConnectionOpen {
                profile: profile.clone(),
            },
            Self::Connection {
                what: ConnectionCommand::Close,
            } => Request::ConnectionClose { connection },
            Self::Schema {
                what: SchemaCommand::List,
            } => Request::NamespaceList { connection },
            Self::Table {
                what: TableCommand::List { path },
            } => Request::TableList {
                connection,
                namespace: path.segments(),
            },
            Self::Query {
                what: QueryCommand::Estimate { sql },
            } => Request::QueryEstimate {
                connection,
                sql: sql.clone(),
            },
            Self::Query {
                what: QueryCommand::Run { sql, max_bytes },
            } => Request::QueryRun {
                connection,
                sql: sql.clone(),
                max_bytes: *max_bytes,
            },
            Self::Query {
                what: QueryCommand::Status { query, limit },
            } => Request::QueryStatus {
                query: query.clone(),
                limit: *limit,
            },
            Self::Query {
                what:
                    QueryCommand::Wait {
                        query,
                        timeout_ms,
                        limit,
                    },
            } => Request::QueryWait {
                query: query.clone(),
                timeout_ms: *timeout_ms,
                limit: *limit,
            },
            Self::Query {
                what: QueryCommand::Cancel { query },
            } => Request::QueryCancel {
                query: query.clone(),
            },
            // Never asked for: `run` sends this one to the MCP server instead
            // of to a session, and `is_mcp` is what stops it getting here.
            Self::Mcp => Request::Snapshot {},
            Self::Table {
                what: TableCommand::Describe { path, refresh },
            } => Request::TableDescribe {
                connection,
                table: path.segments(),
                refresh: *refresh,
            },
            Self::Table {
                what: TableCommand::Preview { path, sort, limit },
            } => Request::TablePreview {
                connection,
                table: path.segments(),
                sort: sort.map(|column| sqlake_api::SortBy { column }),
                limit: *limit,
            },
        }
    }

    /// Whether getting the connection wrong cannot be taken back.
    ///
    /// Only closing, today. It is not part of [`Needs`] because that says what
    /// a command *needs*, and this is about what it does with it.
    const fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::Connection {
                what: ConnectionCommand::Close
            }
        )
    }

    /// Whether this is the long-running MCP server rather than one request.
    ///
    /// Its own question because everything else about a command — the request
    /// it names, what that needs — is about answering once, and this one never
    /// does.
    pub(crate) const fn is_mcp(&self) -> bool {
        matches!(self, Self::Mcp)
    }

    /// What answering this needs, which differs by mode.
    ///
    /// One boolean cannot say it: `connection list` needs a connection to exist
    /// in one-shot, because a store that just started has nothing to list, but
    /// carries no connection id and needs no lookup when attached.
    pub(crate) const fn needs(&self) -> Needs {
        match self {
            // The schema describes the build rather than a session. Opening a
            // connection to print it would put a keyring prompt in front of
            // somebody asking what the commands are.
            Self::Api {
                what: ApiCommand::Schema,
            } => Needs::Nothing,
            Self::Api {
                what: ApiCommand::Snapshot,
            }
            | Self::Connection {
                what: ConnectionCommand::List,
            } => Needs::Session,
            // A store that dies with this process is not a session to open a
            // connection in — see `Needs::LiveSession`.
            Self::Connection {
                what: ConnectionCommand::Open { .. },
            } => Needs::LiveSession,
            // Closing is the same no-op in one-shot as opening: the store dies
            // with the process either way, so the connection it would close is
            // one it had to open first.
            Self::Connection {
                what: ConnectionCommand::Close,
            } => Needs::LiveConnection,
            Self::Schema { .. } | Self::Table { .. } => Needs::Connection,
            // Estimating names a connection and answers a number, which a
            // store that dies afterwards can still do.
            Self::Query {
                what: QueryCommand::Estimate { .. },
            } => Needs::Connection,
            // Running names one too, but answers a handle — and a handle
            // nothing can ask about afterwards is the same no-op `connection
            // open` is refused for. Whether any rows come back at all would
            // depend on the driver finishing inside one settle, which is a
            // race to report as success.
            Self::Query {
                what: QueryCommand::Run { .. },
            } => Needs::LiveConnection,
            // The other three name a query the session already started, and a
            // one-shot store started none.
            Self::Query { .. } => Needs::LiveSession,
            // A session, because every tool it offers needs one — and it holds
            // the backend open for as long as a client is talking to it, so a
            // local store here is a store that lives, unlike a one-shot's.
            Self::Mcp => Needs::Session,
        }
    }
}

/// What a command has to have before it can be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Needs {
    /// Nothing at all: the answer is a fact about this build.
    Nothing,
    /// A session to ask. One-shot has to start one; attached already has one.
    Session,
    /// A session that outlives the command.
    ///
    /// One-shot cannot answer these at all: its store dies with the process, so
    /// a connection opened in it is one nothing can use afterwards. Refused
    /// rather than answered, because "it worked" is the wrong thing to say
    /// about a no-op.
    LiveSession,
    /// A particular connection, named in the request.
    Connection,
    /// Both: a connection to name, in a session that outlives the command.
    LiveConnection,
}

impl Needs {
    /// Whether a store that dies with the command can answer this at all.
    const fn outlives_the_command(self) -> bool {
        matches!(self, Self::LiveSession | Self::LiveConnection)
    }

    /// Whether the request carries a connection id to be chosen first.
    const fn names_a_connection(self) -> bool {
        matches!(self, Self::Connection | Self::LiveConnection)
    }
}

/// A store with nothing to connect to.
///
/// What a command that [needs nothing](Command::needs) runs against. Reading
/// the config to answer `api schema` would let a `connections.toml` that does
/// not parse withhold the document describing how to write one — from the
/// caller least able to guess.
#[derive(Debug)]
pub(crate) struct NoProfiles;

impl Profiles for NoProfiles {
    fn list(&self) -> Vec<ProfileSummary> {
        Vec::new()
    }

    fn resolve(&self, id: &ProfileId) -> Result<ResolvedProfile, ProfileError> {
        Err(ProfileError::new(format!("no profile called `{id}`")))
    }
}

/// Run one command against a store this process starts and stops.
///
/// The store is dropped at the end rather than shut down gracefully: nothing
/// here has state worth flushing, and a connection whose profile is sitting on
/// a keyring dialog would otherwise hold the exit open behind a window nobody
/// is looking at.
pub(crate) fn run(
    command: &Command,
    drivers: Drivers,
    profiles: Arc<dyn Profiles>,
    settings: &Settings,
    connect: Option<ProfileId>,
    session: Option<PathBuf>,
) -> Result<std::process::ExitCode> {
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;

    // The MCP server is not one request, so it does not go through the
    // attach-or-one-shot dance below — it does the same choice itself and then
    // holds whichever it got for as long as a client is talking to it.
    if command.is_mcp() {
        let served = runtime.block_on(serve_mcp(drivers, profiles, settings, connect, session));
        // Not dropped: dropping the runtime waits for blocking tasks, and
        // resolving a profile is one — so a keyring dialog nobody is looking at
        // would hold the process up after the client has already gone.
        runtime.shutdown_background();
        return served;
    }

    let response = runtime.block_on(async {
        // Attached first, because reusing a session is the whole reason the
        // socket exists: its connections, its tunnels and its already-answered
        // credential prompts are what a fresh store would have to pay for
        // again — and for a profile that needs a person, cannot.
        //
        // Not for a command that needs nothing: `api schema` is generated from
        // this build's types, so asking a session would describe whichever
        // binary happens to be running it, and would hang behind a session
        // that is wedged — for an answer this process already has.
        if command.needs() != Needs::Nothing
            && let Some(path) = session
            && let Some(mut client) = sqlake_api::Client::attach(&path)
                .await
                .with_context(|| format!("reaching the session at {}", path.display()))?
        {
            return attached(&mut client, command, connect.as_ref()).await;
        }
        one_shot(command, drivers, profiles, settings, connect).await
    });
    // The runtime goes without waiting for anything still resolving a profile,
    // for the reason in the doc comment above.
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            runtime.shutdown_background();
            return Err(error);
        }
    };
    runtime.shutdown_background();

    print(&response)
}

/// Speak MCP on stdin and stdout until the client goes away.
///
/// Attached when a session answers, and a store of its own otherwise. Unlike a
/// one-shot command, a local store here *does* outlive the requests made
/// against it — the process stays up — so opening connections and starting
/// queries in one is not the no-op it would be for `sqlake query run`.
async fn serve_mcp(
    drivers: Drivers,
    profiles: Arc<dyn Profiles>,
    settings: &Settings,
    connect: Option<ProfileId>,
    session: Option<PathBuf>,
) -> Result<std::process::ExitCode> {
    let backend = sqlake_api::Backend::attach_or_start(session.as_deref(), || {
        Service::new(Store::spawn(
            Wiring::new(drivers, profiles)
                .page_size(settings.page_size)
                .budget(settings.max_bytes_billed)
                .library(crate::library()),
        ))
        .with_max_bytes(settings.agent_max_bytes_billed)
    })
    .await
    .context("reaching a session")?;

    // A local store has nothing open, and every tool but `connection_open`
    // needs a connection. Opened here rather than left to the client, so an
    // agent's first turn is a question about the database rather than about
    // this process.
    if let Some(service) = backend.local() {
        // Nothing here is fatal, including "no profiles are configured": the
        // client can still call `connection_open`, and a server that refused to
        // start would leave an agent with no way to find that out. Said on
        // stderr, because stdout belongs to the protocol.
        match open(service.store(), connect.clone()).await {
            Ok(Ok(_)) => {}
            Ok(Err(failure)) => eprintln!("no connection opened: {}", diagnostic(&failure)),
            Err(why) => eprintln!("no connection opened: {why:#}"),
        }
    }

    sqlake_mcp::Server::new(backend, connect.map(|id| id.as_str().to_owned()))
        .serve_stdio()
        .await
        .map_err(|why| anyhow::anyhow!("the MCP server stopped: {why}"))?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// Ask a session that is already running.
///
/// The connection is chosen here rather than named by the caller: a person
/// opened it, so its id is something only the session knows. `--connect` picks
/// among them by profile when more than one is open.
async fn attached(
    client: &mut sqlake_api::Client,
    command: &Command,
    connect: Option<&ProfileId>,
) -> Result<Response> {
    // Not for `LiveSession`: opening a connection names a profile rather than
    // a connection, so there is nothing to choose.
    let connection = if command.needs().names_a_connection() {
        let open = match client.request(&Request::ConnectionList {}).await? {
            Response::Connections(open) => open,
            // The session answered something else, which is a protocol
            // failure rather than this caller's to explain away.
            other => return Ok(Response::Failed(unexpected(&other))),
        };
        match sqlake_api::choose(
            &open,
            connect.map(ProfileId::as_str),
            command.is_destructive(),
        ) {
            Ok(id) => id,
            // The protocol's message says which connections there are and not
            // how to choose between them, because it is read over a socket by
            // clients that have no flags. This layer has one.
            Err(failure) => return Ok(Response::Failed(with_the_flag(failure))),
        }
    } else {
        String::new()
    };
    client
        .request(&command.request(connection))
        .await
        .context("asking the session")
}

/// The same refusal, with the flag that answers it.
///
/// Recognised by [`sqlake_api::AMBIGUOUS`] rather than by a phrase written here
/// as well, so a reworded refusal is a compile-time concern rather than a hint
/// that quietly stops appearing.
fn with_the_flag(failure: Failure) -> Failure {
    match failure {
        Failure::Unsupported { message } if message.contains(sqlake_api::AMBIGUOUS) => {
            Failure::Unsupported {
                message: format!("{message}: `--connect <profile>`"),
            }
        }
        other => other,
    }
}

fn unexpected(response: &Response) -> Failure {
    Failure::Malformed {
        message: format!("the session answered a connection list with {response:?}"),
    }
}

/// Start a store, answer one request, and drop it.
async fn one_shot(
    command: &Command,
    drivers: Drivers,
    profiles: Arc<dyn Profiles>,
    settings: &Settings,
    connect: Option<ProfileId>,
) -> Result<Response> {
    // Before the store is even started: what this refuses is not a failure of
    // the store, and starting one to say so would open a connection first.
    if command.needs().outlives_the_command() {
        return Ok(Response::Failed(Failure::Unsupported {
            message: "this needs a session that outlives the command — start one with \
                      `sqlake --session <name>` and try again"
                .to_owned(),
        }));
    }
    // Both ceilings, the way an attached session has both: the store applies
    // the session's and the service applies the agent's on top, so a one-shot
    // run refuses exactly what the same command would be refused over a socket.
    let service = Service::new(Store::spawn(
        Wiring::new(drivers, profiles)
            .page_size(settings.page_size)
            .budget(settings.max_bytes_billed)
            .library(crate::library()),
    ))
    .with_max_bytes(settings.agent_max_bytes_billed);
    let connection = if command.needs() == Needs::Nothing {
        String::new()
    } else {
        match open(service.store(), connect).await? {
            Ok(connection) => connection,
            Err(failure) => return Ok(Response::Failed(failure)),
        }
    };
    Ok(service.answer(&command.request(connection)).await)
}

/// Open the one connection a one-shot command reads through.
///
/// One, not every configured profile: opening the rest would put a connection
/// attempt and possibly a credential prompt in front of a caller that asked
/// about a single table.
///
/// A wait that runs out is a [`Failure`] rather than an error, because it is
/// the same event the service reports as one — a caller that parses stdout for
/// `{"error": "timeout"}` should not get an empty stream because the clock ran
/// out three lines earlier. Having nothing to connect to at all is different:
/// no request was attempted, and the answer is a sentence about the config.
async fn open(store: &Store, connect: Option<ProfileId>) -> Result<Result<String, Failure>> {
    let profile = match connect {
        Some(id) => id,
        None => store
            .snapshot()
            .profiles
            .first()
            .map(|p| p.id.clone())
            .context("no connection profiles are configured")?,
    };
    let conn = ConnId::new();
    // Settled here rather than left to the request: `dispatch` only queues, so
    // returning the id straight away hands the service a connection that is not
    // in any snapshot yet — and "no such connection" is what it would say about
    // the one this command just opened.
    let waited = store
        .dispatch_and_settle(
            Action::Connect { profile, conn },
            sqlake_api::DEFAULT_TIMEOUT,
            |s| s.connection_settled(conn),
        )
        .await;
    Ok(match waited {
        Ok(_) => Ok(conn.to_string()),
        Err(sqlake_app::wait::WaitError::TimedOut) => Err(Failure::Timeout {
            waited_ms: u64::try_from(sqlake_api::DEFAULT_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        }),
        Err(sqlake_app::wait::WaitError::Stopped) => Err(Failure::Driver {
            message: "the session stopped while the connection was opening".to_owned(),
        }),
    })
}

/// Print the response, and say in the exit status whether it was a failure.
///
/// Both, rather than one or the other. The JSON is the whole answer, so it is
/// printed for a failure too — a caller that reads stdout gets the reason
/// rather than an empty stream. The status is there so a shell script can
/// branch without a JSON parser, which is the thing `set -e` is already
/// watching.
fn print(response: &Response) -> Result<std::process::ExitCode> {
    let json = serde_json::to_string_pretty(response).context("serialising the response")?;
    println!("{json}");

    match went_wrong(response) {
        Some(why) => {
            eprintln!("{why}");
            Ok(std::process::ExitCode::FAILURE)
        }
        None => Ok(std::process::ExitCode::SUCCESS),
    }
}

/// Why this answer is not a success, if it is not one.
///
/// Split from the printing because `ExitCode` can be neither compared nor
/// displayed, so a test of the decision has to be a test of something else.
fn went_wrong(response: &Response) -> Option<String> {
    match response {
        Response::Failed(failure) => Some(diagnostic(failure)),
        // A connection that would not open is answered *as* a connection, on
        // purpose — but it is not a success. An agent running `connection open
        // prod && table list` would otherwise carry on against a database it
        // never reached, which is exactly what the status is there to say.
        Response::Connection(conn) => match &conn.status {
            Status::Failed { reason } => Some(reason.clone()),
            _ => None,
        },
        // A query the server refused is a failure whatever the transport
        // thinks. `NeedsApproval` is not one: it is an answer, and the caller
        // is supposed to take the number to a person.
        Response::Query(query) => match &query.state {
            QueryState::Failed { message, at } => Some(match at {
                Some(at) => format!("{message} (line {}, column {})", at.line, at.column),
                None => message.clone(),
            }),
            _ => None,
        },
        _ => None,
    }
}

/// The one-line version, for a person watching the terminal.
fn diagnostic(failure: &Failure) -> String {
    match failure {
        Failure::NoSuchConnection { connection } => {
            format!("no connection called `{connection}` is open")
        }
        Failure::NoSuchQuery { query } => {
            format!("this session started no query called `{query}`")
        }
        Failure::NoSuchProfile { profile } => {
            format!("no profile called `{profile}` is configured in connections.toml")
        }
        Failure::NotFound { path } => format!("`{}` is not in this connection", path.join(".")),
        Failure::Driver { message } => message.clone(),
        Failure::Timeout { waited_ms } => {
            format!(
                "gave up after {waited_ms}ms. A profile that needs a prompt has to be opened in a session"
            )
        }
        Failure::Unsupported { message } | Failure::Malformed { message } => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use sqlake_api::{ConnectionInfo, QueryInfo};

    /// Parses the way the real binary does, so the subcommand tree is checked
    /// rather than described.
    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("sqlake").chain(args.iter().copied()))
            .expect("the arguments parse")
            .command
    }

    #[test]
    fn a_refusal_to_guess_names_the_flag_that_answers_it() {
        // The protocol's message says which connections there are and not how
        // to pick between them, because an MCP client has no flags. A person at
        // a terminal does, and this is the layer that knows it.
        let two = [connection("mock", 0), connection("prod", 1)];
        let refused = sqlake_api::choose(&two, None, true).expect_err("closing should refuse");
        let Failure::Unsupported { message } = with_the_flag(refused) else {
            panic!("the refusal should survive being annotated");
        };
        assert!(message.contains("--connect"), "{message}");

        // And nothing else is annotated: only the one refusal a flag answers.
        let other = Failure::Unsupported {
            message: "this driver cannot cancel".to_owned(),
        };
        assert_eq!(with_the_flag(other.clone()), other);
    }

    fn connection(profile: &str, index: usize) -> ConnectionInfo {
        ConnectionInfo {
            id: format!("id-{index}"),
            profile: profile.to_owned(),
            name: profile.to_owned(),
            driver: "mock".into(),
            status: Status::Ready,
            capabilities: None,
        }
    }

    #[test]
    fn a_connection_that_would_not_open_is_not_a_success() {
        // An agent running `connection open prod && table list` must not carry
        // on against a database it never reached.
        let failed = ConnectionInfo {
            id: "id".into(),
            profile: "prod".into(),
            name: "prod".into(),
            driver: "postgres".into(),
            status: Status::Failed {
                reason: "password authentication failed".into(),
            },
            capabilities: None,
        };
        assert_eq!(
            went_wrong(&Response::Connection(failed)).as_deref(),
            Some("password authentication failed")
        );
    }

    #[test]
    fn a_connection_that_opened_is_a_success() {
        let ready = ConnectionInfo {
            id: "id".into(),
            profile: "prod".into(),
            name: "prod".into(),
            driver: "postgres".into(),
            status: Status::Ready,
            capabilities: None,
        };
        assert_eq!(went_wrong(&Response::Connection(ready.clone())), None);
        // And closing one is what a caller asked for, not a failure.
        assert_eq!(
            went_wrong(&Response::Connection(ConnectionInfo {
                status: Status::Closed,
                ..ready
            })),
            None
        );
    }

    #[test]
    fn opening_a_connection_names_a_profile_and_closing_names_a_connection() {
        assert_eq!(
            parse(&["connection", "open", "prod-pg"]).request(String::new()),
            Request::ConnectionOpen {
                profile: "prod-pg".into(),
            }
        );
        // `close` takes no argument: which connection is chosen the way every
        // other command chooses one, and `--connect` narrows it.
        assert_eq!(
            parse(&["connection", "close"]).request("c".into()),
            Request::ConnectionClose {
                connection: "c".into(),
            }
        );
    }

    #[tokio::test]
    async fn one_shot_refuses_what_would_not_outlive_it() {
        // A store that dies with the process is not a session to open a
        // connection in, and "it worked" is the wrong thing to say about a
        // no-op. Closing is the same no-op from the other end: one-shot would
        // have to open a connection before it had one to close.
        for argv in [
            ["connection", "open", "mock"].as_slice(),
            ["connection", "close"].as_slice(),
        ] {
            let response = one_shot(
                &parse(argv),
                Drivers::new().with(Arc::new(sqlake_driver_mock::MockDriver::default())),
                Arc::new(sqlake_driver_mock::MockProfiles::default()),
                &Settings::default(),
                None,
            )
            .await
            .expect("it answers rather than erroring");

            let Response::Failed(Failure::Unsupported { message }) = response else {
                panic!("{argv:?}: {response:?}");
            };
            assert!(message.contains("--session"), "{message}");
        }
    }

    #[tokio::test]
    async fn one_shot_still_answers_everything_that_does_not_outlive_it() {
        // The refusal above is about one thing, not about one-shot mode.
        let response = one_shot(
            &parse(&["connection", "list"]),
            Drivers::new().with(Arc::new(sqlake_driver_mock::MockDriver::default())),
            Arc::new(sqlake_driver_mock::MockProfiles::default()),
            &Settings::default(),
            None,
        )
        .await
        .expect("it answers");
        assert!(matches!(response, Response::Connections(_)), "{response:?}");
    }

    #[test]
    fn the_row_limit_is_on_the_request_that_reads_the_rows() {
        // Not on `run`, which answers a handle and never rows — a limit there
        // would be an argument that does nothing.
        assert_eq!(
            parse(&["query", "run", "select 1"]).request("c".into()),
            Request::QueryRun {
                connection: "c".into(),
                sql: "select 1".into(),
                max_bytes: None,
            }
        );
        assert_eq!(
            parse(&["query", "wait", "q", "--limit", "5", "--timeout-ms", "100"])
                .request(String::new()),
            Request::QueryWait {
                query: "q".into(),
                timeout_ms: Some(100),
                limit: Some(5),
            }
        );
        assert_eq!(
            parse(&["query", "status", "q", "--limit", "5"]).request(String::new()),
            Request::QueryStatus {
                query: "q".into(),
                limit: Some(5),
            }
        );
    }

    #[tokio::test]
    async fn one_shot_can_estimate_a_query_but_not_run_one() {
        // Estimating answers a number, which a store that dies afterwards can
        // still do. Running answers a handle, and a handle nothing can ask
        // about is the same no-op `connection open` is refused for.
        let response = one_shot(
            &parse(&["query", "estimate", "select * from public.users"]),
            Drivers::new().with(Arc::new(sqlake_driver_mock::MockDriver::default())),
            Arc::new(sqlake_driver_mock::MockProfiles::default()),
            &Settings::default(),
            None,
        )
        .await
        .expect("it answers");
        assert!(matches!(response, Response::Query(_)), "{response:?}");

        for argv in [
            &["query", "run", "select 1"][..],
            &["query", "status", "q"][..],
        ] {
            let response = one_shot(
                &parse(argv),
                Drivers::new().with(Arc::new(sqlake_driver_mock::MockDriver::default())),
                Arc::new(sqlake_driver_mock::MockProfiles::default()),
                &Settings::default(),
                None,
            )
            .await
            .expect("it answers");
            let Response::Failed(Failure::Unsupported { message }) = response else {
                panic!("{argv:?} answered {response:?}");
            };
            assert!(message.contains("--session"), "{message}");
        }
    }

    #[test]
    fn a_query_the_server_refused_is_not_a_success() {
        let failed = QueryInfo {
            id: "q".into(),
            connection: "c".into(),
            sql: "select nope".into(),
            estimate: None,
            state: QueryState::Failed {
                message: "no such column: nope".into(),
                at: Some(sqlake_api::PositionInfo { line: 1, column: 8 }),
            },
        };
        let why = went_wrong(&Response::Query(failed)).expect("a failure");
        assert!(why.contains("no such column"), "{why}");
        // The position too: an agent that has to re-read its own SQL to find
        // where it went wrong has been told less than the server said.
        assert!(why.contains("line 1"), "{why}");
    }

    #[test]
    fn a_query_waiting_for_a_person_is_not_a_failure() {
        // Over the budget is an answer, and the caller is supposed to take the
        // number to somebody. Exiting non-zero would make a shell script treat
        // a question as a crash.
        let asking = QueryInfo {
            id: "q".into(),
            connection: "c".into(),
            sql: "select * from big".into(),
            estimate: Some(sqlake_api::EstimateInfo::Bytes {
                bytes: 5_000_000_000,
            }),
            state: QueryState::NeedsApproval {
                budget: 1_000_000_000,
            },
        };
        assert_eq!(went_wrong(&Response::Query(asking)), None);
    }

    #[test]
    fn a_dotted_path_is_a_path() {
        let command = parse(&["table", "preview", "public.users"]);
        assert_eq!(
            command.request("c".into()),
            Request::TablePreview {
                connection: "c".into(),
                table: vec!["public".into(), "users".into()],
                sort: None,
                limit: None,
            }
        );
    }

    #[test]
    fn describing_asks_the_driver_again_only_when_told_to() {
        assert_eq!(
            parse(&["table", "describe", "public.users"]).request("c".into()),
            Request::TableDescribe {
                connection: "c".into(),
                table: vec!["public".into(), "users".into()],
                refresh: false,
            }
        );
        assert!(matches!(
            parse(&["table", "describe", "public.users", "--refresh"]).request("c".into()),
            Request::TableDescribe { refresh: true, .. }
        ));
    }

    #[test]
    fn a_name_containing_a_dot_can_still_be_named() {
        // The reason the protocol carries a path rather than a name to rejoin.
        // Splitting `my.schema` would ask for a table in a namespace nobody
        // has, and the answer would be a plausible-looking "not found".
        let command = parse(&["table", "list", "--part", "my.schema"]);
        assert_eq!(
            command.request("c".into()),
            Request::TableList {
                connection: "c".into(),
                namespace: vec!["my.schema".into()],
            }
        );
    }

    #[test]
    fn a_path_is_required_where_one_is_needed() {
        assert!(Cli::try_parse_from(["sqlake", "table", "preview"]).is_err());
    }

    #[test]
    fn the_two_ways_of_giving_a_path_cannot_be_mixed() {
        assert!(
            Cli::try_parse_from(["sqlake", "table", "list", "public", "--part", "x"]).is_err(),
            "a path given twice would silently use one of them"
        );
    }

    #[test]
    fn every_subcommand_names_a_request() {
        let commands = [
            (parse(&["api", "snapshot"]), Request::Snapshot {}),
            (parse(&["api", "schema"]), Request::Schema {}),
            (parse(&["connection", "list"]), Request::ConnectionList {}),
            (
                parse(&["schema", "list"]),
                Request::NamespaceList {
                    connection: "c".into(),
                },
            ),
        ];
        for (command, expected) in commands {
            assert_eq!(command.request("c".into()), expected);
        }
    }

    #[test]
    fn what_a_command_needs_is_not_one_question() {
        // `connection list` needs a connection to exist before one-shot has
        // anything to report, and needs no id at all when attached. A single
        // boolean answered one of those and got the other wrong.
        assert_eq!(parse(&["api", "schema"]).needs(), Needs::Nothing);
        assert_eq!(parse(&["api", "snapshot"]).needs(), Needs::Session);
        assert_eq!(parse(&["connection", "list"]).needs(), Needs::Session);
        assert_eq!(parse(&["schema", "list"]).needs(), Needs::Connection);
        assert_eq!(
            parse(&["table", "preview", "public.users"]).needs(),
            Needs::Connection
        );
    }

    #[test]
    fn a_limit_reaches_the_request() {
        let command = parse(&["table", "preview", "public.users", "--limit", "3"]);
        assert!(matches!(
            command.request("c".into()),
            Request::TablePreview { limit: Some(3), .. }
        ));
    }
}
