//! The PostgreSQL driver.
//!
//! One [`PgDriver`] serves every PostgreSQL profile: what distinguishes two
//! live connections is the [`ResolvedProfile`] each was opened with.
//!
//! The rule this crate follows above all others is that **an unknown type must
//! not be able to fail a query**. Every value arrives as bytes and is decoded
//! on the way out, so a PostGIS geometry or somebody's enum shows up as text
//! rather than as an error where a table used to be — see [`value`].

pub mod config;
pub mod tls;
pub mod value;

use async_trait::async_trait;
use sqlake_core::capability::{Capabilities, DriverKind, HierarchyLevel, QuoteStyle};
use sqlake_core::driver::{Driver, DriverError, DriverResult, Session};
use sqlake_core::node::{NodeKind, NodeRef, TableRef, TreeNode};
use sqlake_core::profile::{Params, ResolvedProfile};
use sqlake_core::result::{PageRequest, ResultSet};
use tokio_postgres::{Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

pub use value::RawValue;

/// Database, schema, relation. Three levels, where the mock has two — which is
/// the point of the level list being data rather than an assumption.
pub const HIERARCHY: &[HierarchyLevel] = &[
    HierarchyLevel::new(NodeKind::Catalog, "database"),
    HierarchyLevel::new(NodeKind::Namespace, "schema"),
    HierarchyLevel::new(NodeKind::Relation, "table"),
];

/// What PostgreSQL can do, as the UI needs to know it.
pub const CAPABILITIES: Capabilities = Capabilities {
    hierarchy: HIERARCHY,
    indexes: true,
    triggers: true,
    constraints: true,
    partitioning: true,
    transactions: true,
    cancel: true,
    streaming: true,
    // `EXPLAIN` gives a row estimate. It costs nothing to ask, unlike
    // BigQuery's dry run, but it is an estimate in both cases.
    cost_estimate: true,
    // Every preview is a `SELECT`, so previewing is querying. BigQuery is the
    // driver where this is true the other way round.
    free_preview: false,
    quote_style: QuoteStyle::DoubleQuote,
};

#[derive(Debug)]
pub struct PgDriver {
    /// The longest a connection attempt may take, over everything.
    deadline: std::time::Duration,
}

impl Default for PgDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PgDriver {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            deadline: config::DEADLINE,
        }
    }

    /// A driver that gives up sooner.
    ///
    /// Exists because the interesting case — a host that accepts the
    /// connection and then says nothing — is only observable by waiting, and
    /// waiting [`config::DEADLINE`] to find that out is not a test anyone runs
    /// twice.
    #[must_use]
    pub const fn with_deadline(deadline: std::time::Duration) -> Self {
        Self { deadline }
    }
}

#[async_trait]
impl Driver for PgDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self, profile: &ResolvedProfile) -> DriverResult<Box<dyn Session>> {
        let Params::Postgres(params) = &profile.params else {
            return Err(DriverError::Connect(format!(
                "profile `{}` is not a postgres profile",
                profile.id
            )));
        };

        let config = config::build(profile, params);
        let opened = open(&config, tls::Verification::of(params.sslmode));

        // `Config::connect_timeout` reaches the socket and nothing else — see
        // `config::DEADLINE`. A stale tunnel or a load balancer with no backend
        // accepts the connection and then says nothing, and without this the
        // future stays pending for ever.
        let client = tokio::time::timeout(self.deadline, opened)
            .await
            .map_err(|_| {
                DriverError::Connect(format!(
                    "no answer from {}:{} within {:?}",
                    params.host, params.port, self.deadline
                ))
            })??;

        Ok(Box::new(PgSession { client }))
    }
}

/// The handshake, with no deadline of its own — [`PgDriver::connect`] puts one
/// around the whole of it.
async fn open(config: &Config, verification: Option<tls::Verification>) -> DriverResult<Client> {
    match verification {
        // `disable` is the one mode with no TLS stack to configure, and asking
        // rustls for a connector that will never be used would fail on a
        // machine with no trust store for no reason.
        None => {
            let (client, connection) = config
                .connect(tokio_postgres::NoTls)
                .await
                .map_err(connect_failed)?;
            spawn(connection);
            Ok(client)
        }
        Some(verification) => {
            let connector = MakeRustlsConnect::new(tls::client_config(verification)?);
            let (client, connection) = config.connect(connector).await.map_err(connect_failed)?;
            spawn(connection);
            Ok(client)
        }
    }
}

/// The connection future is the thing that actually moves bytes; the `Client`
/// only talks to it. Dropping it without spawning would leave a client that
/// answers nothing, for ever.
fn spawn<S, T>(connection: tokio_postgres::Connection<S, T>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            // Not a panic and not a toast: the session sees this as its next
            // call failing, which is where the user is looking.
            tracing::warn!(%err, "postgres connection ended");
        }
    });
}

/// Connection failures are the ones a user reads most often, so they keep the
/// server's own words — "password authentication failed", "no pg_hba.conf
/// entry" — rather than being summarised.
///
/// Which means walking the cause chain: `tokio_postgres::Error` displays only
/// its category ("db error", "error connecting to server") and hangs the
/// reason off [`Error::source`](std::error::Error::source), so `to_string`
/// alone throws away every word worth reading.
fn connect_failed(err: tokio_postgres::Error) -> DriverError {
    use std::error::Error as _;
    use std::fmt::Write as _;

    let mut message = err.to_string();
    let mut cause = err.source();
    while let Some(next) = cause {
        let _ = write!(message, ": {next}");
        cause = next.source();
    }
    DriverError::Connect(message)
}

#[derive(Debug)]
pub struct PgSession {
    #[allow(
        dead_code,
        reason = "the catalogue queries that use it arrive in the next task"
    )]
    client: Client,
}

#[async_trait]
impl Session for PgSession {
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn children(&self, _of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
        Err(unimplemented_yet("walking the object tree"))
    }

    async fn preview(&self, _table: &TableRef, _req: &PageRequest) -> DriverResult<ResultSet> {
        Err(unimplemented_yet("previewing a relation"))
    }

    async fn close(self: Box<Self>) {
        // Dropping the client is the whole goodbye. `tokio-postgres` sends the
        // `Terminate` itself when the request channel closes, and the spawned
        // connection task then ends on its own; there is nothing left here to
        // await that would not simply be waiting for a drop.
    }
}

/// The half of this driver that T5 fills in.
///
/// A named error rather than a `todo!()`: this driver is reachable from the
/// binary the moment it is registered, and a panic in a spawned task is a
/// worse answer than a row that says what is missing.
fn unimplemented_yet(what: &str) -> DriverError {
    DriverError::Unsupported(format!("{what} is not implemented yet"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlake_core::id::ProfileId;
    use sqlake_core::profile::PostgresParams;

    fn profile(params: Params) -> ResolvedProfile {
        ResolvedProfile {
            id: ProfileId::parse("prod-pg").expect("a usable id"),
            readonly: false,
            params,
        }
    }

    #[test]
    fn the_capabilities_now_have_more_than_one_answer() {
        // Why `Capabilities` exists at all. Until this driver, every field the
        // UI reads had exactly one value in the whole workspace — a UI that
        // branched on the driver instead would have looked perfectly correct.
        let mock = sqlake_driver_mock::CAPABILITIES;
        assert_ne!(CAPABILITIES.hierarchy.len(), mock.hierarchy.len());
        assert_ne!(CAPABILITIES.indexes, mock.indexes);
        assert_ne!(CAPABILITIES.free_preview, mock.free_preview);
    }

    #[test]
    fn a_postgres_namespace_is_called_a_schema() {
        // The label the tree shows comes from here, so that neither "schema"
        // nor "dataset" ever reaches a `match` in the UI.
        assert_eq!(CAPABILITIES.label_for(NodeKind::Namespace), Some("schema"));
        assert_eq!(CAPABILITIES.label_for(NodeKind::Catalog), Some("database"));
        assert_eq!(CAPABILITIES.depth(), 3);
    }

    #[tokio::test]
    async fn a_profile_for_another_driver_is_refused_before_any_socket_is_opened() {
        let err = PgDriver::new()
            .connect(&profile(Params::Mock))
            .await
            .expect_err("should not connect");
        assert!(err.to_string().contains("not a postgres profile"), "{err}");
    }

    #[tokio::test]
    async fn a_host_that_accepts_and_then_says_nothing_gives_up() {
        // The failure the socket timeout does not cover: the connection is
        // made, and the handshake never finishes. A stale tunnel or a load
        // balancer with no backend behaves exactly like this listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let port = listener.local_addr().expect("an address").port();
        let _accepting = tokio::spawn(async move {
            let held = listener.accept().await;
            // Hold the connection open and answer nothing at all.
            std::future::pending::<()>().await;
            drop(held);
        });

        let params = PostgresParams {
            host: "127.0.0.1".to_owned(),
            port,
            database: "app".to_owned(),
            user: "readonly".to_owned(),
            sslmode: sqlake_core::profile::SslMode::Disable,
            password: None,
        };
        let err = PgDriver::with_deadline(std::time::Duration::from_millis(150))
            .connect(&profile(Params::Postgres(params)))
            .await
            .expect_err("should give up");
        assert!(err.to_string().contains("no answer from"), "{err}");
    }

    #[tokio::test]
    async fn a_host_that_is_not_there_says_so_rather_than_hanging() {
        // Port 1 on the loopback: nothing listens, and the refusal is
        // immediate, so this exercises the error path without a server.
        let params = PostgresParams {
            host: "127.0.0.1".to_owned(),
            port: 1,
            database: "app".to_owned(),
            user: "readonly".to_owned(),
            sslmode: sqlake_core::profile::SslMode::Disable,
            password: None,
        };
        let err = PgDriver::new()
            .connect(&profile(Params::Postgres(params)))
            .await
            .expect_err("should not connect");
        assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
        assert!(err.is_retryable(), "a refused connection is worth retrying");
        // And it says *why*. `tokio_postgres::Error` displays its category
        // only — "error connecting to server" with the refusal hidden in its
        // source — which is a message nobody can act on.
        let message = err.to_string();
        assert!(message.contains("error connecting to server:"), "{message}");
        // The reason itself, not just the fact that a cause was appended: on
        // both platforms this project runs on, a closed port refuses.
        assert!(message.to_lowercase().contains("refused"), "{message}");
    }
}
