//! A resolved profile, as `tokio-postgres` wants it.
//!
//! Everything the server needs to know about the session is set in the startup
//! packet rather than by `SET` statements afterwards. A `SET` is a round trip,
//! and — more to the point — it leaves a window in which the connection is
//! open and the settings are not yet applied. A read-only connection that is
//! briefly writable is not a read-only connection.

use std::time::Duration;

use sqlake_core::profile::{PostgresParams, ResolvedProfile, SslMode};
use tokio_postgres::Config;
use tokio_postgres::config::SslMode as PgSslMode;

/// Long enough for a bastion or a VPN to answer, short enough that a wrong
/// host is a message rather than a hang.
///
/// This one is per *socket*: a host that resolves to several addresses is
/// tried at each in turn, and each attempt gets it. [`DEADLINE`] is what
/// bounds the whole thing.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest `connect` may take before it gives up, over everything.
///
/// [`CONNECT_TIMEOUT`] reaches the socket and nothing else — name resolution,
/// the TLS handshake and authentication all wait for ever — so a host that
/// accepts the connection and then says nothing would hang a session actor
/// with no way for the UI to get out of it. Generous next to the per-socket
/// timeout, because a host with several addresses is meant to get through more
/// than one of them.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Shows up in `pg_stat_activity`, so a DBA can see which connections are this
/// client's before asking.
pub const APPLICATION_NAME: &str = "sqlake";

/// Build the connection configuration for a profile.
///
/// The password is set here and nowhere else, and `Config`'s own `Debug`
/// redacts it.
#[must_use]
pub fn build(profile: &ResolvedProfile, params: &PostgresParams) -> Config {
    let mut config = Config::new();
    config
        .host(&params.host)
        .port(params.port)
        .dbname(&params.database)
        .user(&params.user)
        .application_name(APPLICATION_NAME)
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(ssl_mode(params.sslmode));

    if let Some(password) = &params.password {
        config.password(password.expose());
    }

    if profile.readonly {
        // The server refuses the write, not us. A client-side check protects
        // nothing that a client-side bug cannot undo, and this one applies to
        // every path into the connection — including the SQL tab in M4 —
        // without any of them having to remember it. It is a *default*, so a
        // deliberate `SET default_transaction_read_only = off` or `BEGIN READ
        // WRITE` still gets through; guarding against a user who types that is
        // a job for the role's own privileges.
        config.options("-c default_transaction_read_only=on");
    }

    config
}

/// libpq's five modes onto the three `tokio-postgres` negotiates.
///
/// The verifying modes are `Require` here: what separates them from `require`
/// is what rustls does with the certificate afterwards, which is
/// [`crate::tls`]'s half of the same decision.
const fn ssl_mode(mode: SslMode) -> PgSslMode {
    match mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => PgSslMode::Require,
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::id::ProfileId;
    use sqlake_core::profile::Params;
    use sqlake_core::secret::Secret;

    use super::*;

    fn params() -> PostgresParams {
        PostgresParams {
            host: "db.internal".to_owned(),
            port: 6432,
            database: "app".to_owned(),
            user: "readonly".to_owned(),
            sslmode: SslMode::VerifyFull,
            password: Some(Secret::new("hunter2".to_owned())),
        }
    }

    fn profile(readonly: bool, params: PostgresParams) -> ResolvedProfile {
        ResolvedProfile {
            id: ProfileId::parse("prod-pg").expect("a usable id"),
            readonly,
            params: Params::Postgres(params),
        }
    }

    fn build_from(readonly: bool, params: PostgresParams) -> Config {
        let profile = profile(readonly, params.clone());
        build(&profile, &params)
    }

    #[test]
    fn the_profile_arrives_intact() {
        let config = build_from(false, params());
        assert_eq!(config.get_ports(), [6432]);
        assert_eq!(config.get_dbname(), Some("app"));
        assert_eq!(config.get_user(), Some("readonly"));
        assert_eq!(config.get_application_name(), Some(APPLICATION_NAME));
        assert_eq!(config.get_connect_timeout(), Some(&CONNECT_TIMEOUT));
        assert_eq!(config.get_password(), Some(&b"hunter2"[..]));
    }

    #[test]
    fn a_profile_with_no_password_sets_none() {
        // Not an empty one: a Unix socket or `trust` authentication has no
        // password, and sending an empty string is a different thing to send.
        let config = build_from(
            false,
            PostgresParams {
                password: None,
                ..params()
            },
        );
        assert_eq!(config.get_password(), None);
    }

    #[test]
    fn read_only_is_asked_for_in_the_startup_packet() {
        // Not as a `SET` afterwards: that leaves the connection open and
        // writable until the statement lands.
        let config = build_from(true, params());
        assert_eq!(
            config.get_options(),
            Some("-c default_transaction_read_only=on")
        );
        assert_eq!(build_from(false, params()).get_options(), None);
    }

    #[test]
    fn the_verifying_modes_still_require_tls() {
        // What `verify-ca` and `verify-full` add happens in rustls; here they
        // are simply "encrypted, no fallback".
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let config = build_from(
                false,
                PostgresParams {
                    sslmode: mode,
                    ..params()
                },
            );
            assert_eq!(config.get_ssl_mode(), PgSslMode::Require, "{mode:?}");
        }
        assert_eq!(
            build_from(
                false,
                PostgresParams {
                    sslmode: SslMode::Prefer,
                    ..params()
                }
            )
            .get_ssl_mode(),
            PgSslMode::Prefer
        );
        assert_eq!(
            build_from(
                false,
                PostgresParams {
                    sslmode: SslMode::Disable,
                    ..params()
                }
            )
            .get_ssl_mode(),
            PgSslMode::Disable
        );
    }

    #[test]
    fn the_password_does_not_appear_in_the_configs_own_debug() {
        // `tokio-postgres` redacts it, and this is the struct that ends up in
        // a connection error.
        let shown = format!("{:?}", build_from(false, params()));
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}
