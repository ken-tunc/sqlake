//! The PostgreSQL driver, through the shared suite, against a real server.
//!
//! Everything else in this crate is pure: the SQL is a string a test can read,
//! and the decoding is bytes in and values out. That leaves exactly the part
//! that shipped broken — a parameter whose type only the server knows — so
//! this is the test that has to be able to fail.
//!
//! It needs Docker. Without it the test says so and passes, because a laptop
//! with no daemon should not turn the whole suite red; with `SQLAKE_REQUIRE_DOCKER`
//! set, the same absence is a failure, because CI skipping this silently is
//! the one outcome that would make it worthless.

use std::sync::Arc;

use sqlake_conformance::Subject;
use sqlake_core::id::ProfileId;
use sqlake_core::node::TableRef;
use sqlake_core::profile::{Params, PostgresParams, ResolvedProfile, SslMode};
use sqlake_driver_postgres::PgDriver;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;

/// The image's own defaults, which `testcontainers-modules` sets up.
const USER: &str = "postgres";
const DATABASE: &str = "postgres";
/// The image sets one, and then refuses connections that arrive without it.
const PASSWORD: &str = "postgres";

/// Four rows, two columns, and a first column that sorts distinctly — the
/// shape the suite documents.
const FIXTURE: &str = "
    CREATE TABLE users (id integer PRIMARY KEY, email text);
    INSERT INTO users VALUES
        (1, 'a@example.com'),
        (2, 'b@example.com'),
        (3, 'c@example.com'),
        (4, 'd@example.com');
    CREATE VIEW recent_users AS SELECT * FROM users WHERE id > 2;
    CREATE TABLE events (at timestamptz, amount numeric(10,2), payload jsonb)
        PARTITION BY RANGE (at);
    CREATE TABLE events_2026 PARTITION OF events
        FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
";

#[tokio::test]
async fn the_postgres_driver_conforms() {
    let Some(container) = start().await else {
        return;
    };
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("a mapped port");

    seed(port).await;

    sqlake_conformance::run(&Subject {
        driver: Arc::new(PgDriver::new()),
        profile: profile(port),
        relation: TableRef::new([DATABASE, "public", "users"]),
        missing: TableRef::new([DATABASE, "public", "no_such_relation"]),
    })
    .await;
}

/// What the tree looks like against a server, which is the half the shared
/// suite deliberately does not know about.
#[tokio::test]
async fn the_tree_shows_what_a_person_would_look_for() {
    use sqlake_core::driver::Driver as _;
    use sqlake_core::node::{NodeKind, NodeRef, RelationKind};

    let Some(container) = start().await else {
        return;
    };
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("a mapped port");
    seed(port).await;

    let session = PgDriver::new()
        .connect(&profile(port))
        .await
        .expect("should connect");

    let schemas = session
        .children(&NodeRef::new(NodeKind::Catalog, [DATABASE]))
        .await
        .expect("should list schemas");
    let names: Vec<&str> = schemas.iter().map(|s| s.label.as_str()).collect();
    assert!(names.contains(&"public"), "{names:?}");
    // Looking at the catalogue is half of what a database client is for.
    assert!(names.contains(&"pg_catalog"), "{names:?}");
    assert!(names.contains(&"information_schema"), "{names:?}");
    // Internal storage is not.
    assert!(
        !names.iter().any(|n| n.starts_with("pg_toast")),
        "{names:?}"
    );

    let relations = session
        .children(&NodeRef::new(NodeKind::Namespace, [DATABASE, "public"]))
        .await
        .expect("should list relations");
    let kinds: Vec<(&str, Option<RelationKind>)> = relations
        .iter()
        .map(|r| (r.label.as_str(), r.relation_kind))
        .collect();

    assert!(
        kinds.contains(&("users", Some(RelationKind::Table))),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("recent_users", Some(RelationKind::View))),
        "{kinds:?}"
    );
    // A partitioned table is a table…
    assert!(
        kinds.contains(&("events", Some(RelationKind::Table))),
        "{kinds:?}"
    );
    // …and its partitions are not separate relations in the tree, or a table
    // partitioned by day would bury everything around it.
    assert!(
        !kinds.iter().any(|(name, _)| *name == "events_2026"),
        "{kinds:?}"
    );

    session.close().await;
}

/// The types this driver decodes, read back through a real server.
///
/// The unit tests encode with `postgres-types` and decode here; this is the
/// round trip that also proves the *server* sends what those tests assumed.
#[tokio::test]
async fn values_survive_the_round_trip() {
    use sqlake_core::driver::Driver as _;
    use sqlake_core::result::PageRequest;
    use sqlake_core::value::Value;

    let Some(container) = start().await else {
        return;
    };
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("a mapped port");
    seed(port).await;

    let client = raw_client(port).await;
    client
        .batch_execute(
            "CREATE TABLE assorted (
                 id integer,
                 price numeric(10,2),
                 ratio real,
                 when_ timestamptz,
                 doc jsonb,
                 gap text,
                 span interval
             );
             INSERT INTO assorted VALUES
                 (1, 12345.67, 1.1, '2026-08-15 12:34:56+00', '{\"a\": [1, 2]}', NULL, '1 day');",
        )
        .await
        .expect("should seed");

    let session = PgDriver::new()
        .connect(&profile(port))
        .await
        .expect("should connect");
    let page = session
        .preview(
            &TableRef::new([DATABASE, "public", "assorted"]),
            &PageRequest::first(),
        )
        .await
        .expect("should preview");

    let row = &page.rows[0];
    assert_eq!(row.get(0), Some(&Value::Int(1)));
    // Every digit, not a float that happens to be close.
    assert_eq!(row.get(1), Some(&Value::Decimal("12345.67".to_owned())));
    // A `real` keeps the digits it was written with.
    assert_eq!(row.get(2), Some(&Value::Float(1.1)));
    assert!(matches!(row.get(3), Some(Value::TimestampTz(_))), "{row:?}");
    assert!(matches!(row.get(4), Some(Value::Json(_))), "{row:?}");
    // NULL is not an empty string.
    assert_eq!(row.get(5), Some(&Value::Null));
    // And a type this driver does not decode is shown rather than refused.
    assert!(
        matches!(row.get(6), Some(Value::Opaque { type_name, .. }) if type_name == "interval"),
        "{row:?}"
    );

    session.close().await;
}

/// `None` when there is no Docker to talk to.
///
/// # Panics
///
/// When `SQLAKE_REQUIRE_DOCKER` is set, because then the absence is the
/// failure: a CI run that quietly skips this suite is a CI run that proves
/// nothing about the driver.
async fn start() -> Option<testcontainers::ContainerAsync<Postgres>> {
    match Postgres::default().start().await {
        Ok(container) => Some(container),
        Err(err) => {
            assert!(
                std::env::var_os("SQLAKE_REQUIRE_DOCKER").is_none(),
                "SQLAKE_REQUIRE_DOCKER is set and no container could be started: {err}"
            );
            eprintln!("skipping: no container runtime ({err})");
            None
        }
    }
}

fn profile(port: u16) -> ResolvedProfile {
    ResolvedProfile {
        id: ProfileId::parse("conformance").expect("a usable id"),
        readonly: false,
        params: Params::Postgres(PostgresParams {
            host: "127.0.0.1".to_owned(),
            port,
            database: DATABASE.to_owned(),
            user: USER.to_owned(),
            // A container that lives for one test has nothing worth
            // encrypting, and the image does not offer TLS anyway.
            sslmode: SslMode::Disable,
            password: Some(sqlake_core::secret::Secret::new(PASSWORD.to_owned())),
        }),
    }
}

async fn raw_client(port: u16) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(port)
        .user(USER)
        .password(PASSWORD)
        .dbname(DATABASE)
        .connect(tokio_postgres::NoTls)
        .await
        .expect("should connect");
    tokio::spawn(connection);
    client
}

async fn seed(port: u16) {
    raw_client(port)
        .await
        .batch_execute(FIXTURE)
        .await
        .expect("should seed");
}
