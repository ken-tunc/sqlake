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
use tokio_postgres::Client;
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

#[derive(Debug, Default)]
pub struct PgDriver;

impl PgDriver {
    #[must_use]
    pub fn new() -> Self {
        Self
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
        let client = match tls::Verification::of(params.sslmode) {
            // `disable` is the one mode with no TLS stack to configure, and
            // asking rustls for a connector that will never be used would fail
            // on a machine with no trust store for no reason.
            None => {
                let (client, connection) = config
                    .connect(tokio_postgres::NoTls)
                    .await
                    .map_err(connect_failed)?;
                spawn(connection);
                client
            }
            Some(verification) => {
                let connector = MakeRustlsConnect::new(tls::client_config(verification)?);
                let (client, connection) =
                    config.connect(connector).await.map_err(connect_failed)?;
                spawn(connection);
                client
            }
        };

        Ok(Box::new(PgSession { client }))
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
fn connect_failed(err: tokio_postgres::Error) -> DriverError {
    DriverError::Connect(err.to_string())
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
        // Dropping the client closes the socket and ends the connection task.
        // There is no goodbye to send: PostgreSQL treats a closed socket as a
        // disconnect, and a `Terminate` message would only be politer to a
        // server that is not waiting for one.
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
    }
}
