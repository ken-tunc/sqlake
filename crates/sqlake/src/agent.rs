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
use sqlake_api::{ConnectionInfo, Failure, Request, Response, Service, Status};
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store};
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
            Self::Schema {
                what: SchemaCommand::List,
            } => Request::NamespaceList { connection },
            Self::Table {
                what: TableCommand::List { path },
            } => Request::TableList {
                connection,
                namespace: path.segments(),
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
            Self::Schema { .. } | Self::Table { .. } => Needs::Connection,
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
    /// A particular connection, named in the request.
    Connection,
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
    page_size: u32,
    connect: Option<ProfileId>,
    session: Option<PathBuf>,
) -> Result<std::process::ExitCode> {
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
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
        one_shot(command, drivers, profiles, page_size, connect).await
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
    let connection = match command.needs() {
        Needs::Nothing | Needs::Session => String::new(),
        Needs::Connection => {
            let open = match client.request(&Request::ConnectionList {}).await? {
                Response::Connections(open) => open,
                // The session answered something else, which is a protocol
                // failure rather than this caller's to explain away.
                other => return Ok(Response::Failed(unexpected(&other))),
            };
            match choose(&open, connect) {
                Ok(id) => id,
                Err(failure) => return Ok(Response::Failed(failure)),
            }
        }
    };
    client
        .request(&command.request(connection))
        .await
        .context("asking the session")
}

/// Which of the session's connections to read through.
///
/// Split from the asking so the decision is testable without a socket: the
/// asking is one request, and the choosing is the part with rules.
fn choose(open: &[ConnectionInfo], connect: Option<&ProfileId>) -> Result<String, Failure> {
    // Everything when nothing was named: a session with two connections open
    // is ordinary, and narrowing that is what `--connect` is for.
    let asked_for = |c: &&ConnectionInfo| connect.is_none_or(|p| c.profile == p.as_str());
    // A ready one ahead of the rest, because a session whose first connection
    // failed to open still has a working second: taking one by position alone
    // would answer with that failure instead of with the database.
    let chosen = open
        .iter()
        .find(|c| asked_for(c) && c.status == Status::Ready)
        .or_else(|| open.iter().find(asked_for));
    chosen.map(|c| c.id.clone()).ok_or_else(|| match connect {
        // The id a caller could have meant is the profile it named, so that is
        // what the failure carries. Which connections *are* open is one
        // `connection list` away and does not belong in this answer.
        Some(profile) => Failure::NoSuchConnection {
            connection: profile.as_str().to_owned(),
        },
        // Not "no such connection": the session is reachable and has none, so
        // there was never an id to get wrong.
        None => Failure::Unsupported {
            message: "the session has no connections open".to_owned(),
        },
    })
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
    page_size: u32,
    connect: Option<ProfileId>,
) -> Result<Response> {
    // No budget: an agent runs nothing yet, and A2 is where the answer
    // to "who says yes for one" is decided rather than assumed here.
    let service = Service::new(Store::spawn(drivers, profiles, page_size, None));
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

    if let Response::Failed(failure) = response {
        eprintln!("{}", diagnostic(failure));
        return Ok(std::process::ExitCode::FAILURE);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// The one-line version, for a person watching the terminal.
fn diagnostic(failure: &Failure) -> String {
    match failure {
        Failure::NoSuchConnection { connection } => {
            format!("no connection called `{connection}` is open")
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

    fn open(profiles: &[&str]) -> Vec<ConnectionInfo> {
        profiles
            .iter()
            .enumerate()
            .map(|(i, profile)| ConnectionInfo {
                id: format!("id-{i}"),
                profile: (*profile).to_owned(),
                name: (*profile).to_owned(),
                driver: "mock".into(),
                status: sqlake_api::Status::Ready,
                capabilities: None,
            })
            .collect()
    }

    #[test]
    fn a_session_with_one_connection_needs_no_choosing() {
        assert_eq!(choose(&open(&["mock"]), None), Ok("id-0".into()));
    }

    #[test]
    fn connect_picks_among_a_sessions_connections() {
        // The point of `--connect` when attached: two databases open is
        // ordinary, and the first is not always the one meant.
        let open = open(&["staging", "prod"]);
        let prod = ProfileId::parse("prod").expect("a usable id");
        assert_eq!(choose(&open, Some(&prod)), Ok("id-1".into()));
    }

    #[test]
    fn a_profile_the_session_has_not_opened_is_named_in_the_failure() {
        let open = open(&["staging"]);
        let prod = ProfileId::parse("prod").expect("a usable id");
        assert_eq!(
            choose(&open, Some(&prod)),
            Err(Failure::NoSuchConnection {
                connection: "prod".into()
            })
        );
    }

    #[test]
    fn a_connection_that_failed_to_open_is_not_the_one_to_read_through() {
        // A session opened with two profiles where the first could not
        // connect: reading through it would answer with that failure, and the
        // database next to it is right there.
        let mut open = open(&["staging", "prod"]);
        open[0].status = Status::Failed {
            reason: "no route to host".into(),
        };
        assert_eq!(choose(&open, None), Ok("id-1".into()));
    }

    #[test]
    fn a_session_with_nothing_open_is_not_a_wrong_id() {
        // `NoSuchConnection` would send the caller looking for a typo in an id
        // it never gave.
        assert!(matches!(
            choose(&open(&[]), None),
            Err(Failure::Unsupported { .. })
        ));
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
