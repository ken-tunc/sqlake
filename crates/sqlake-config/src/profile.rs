//! Connection profiles: what `connections.toml` says, once it has been checked.
//!
//! Parsing happens in two stages. `RawConnection` is the shape on disk — every
//! driver's keys in one flat table, all optional — and [`Profile`] is what
//! survives validation: a driver that exists, with the keys that driver needs
//! and none that belong to another one. The stage in between is where an error
//! can still name the connection, the key and the reason, which is the whole
//! point of doing it in two.
//!
//! **No secret is here.** A profile says *where* the password is; reading it is
//! `Profile → ResolvedProfile`, and that conversion is the only place a secret
//! exists in memory.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sqlake_core::capability::DriverKind;
use sqlake_core::id::ProfileId;
use sqlake_core::profile::SslMode;

use crate::bytes::ByteSize;
use crate::error::{ConfigError, ConfigResult};

/// One connection, as configured. Not yet connectable: see the module note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: ProfileId,
    /// What the UI calls it. Defaults to the id.
    pub name: String,
    pub driver: DriverConfig,
    /// Ask the server to refuse writes on this connection.
    pub readonly: bool,
    /// Colours the connection in the UI. The point is a production tab that
    /// does not look like a scratch one.
    pub color: Option<ProfileColor>,
    /// Names an entry in `tunnels.toml`. Parsed now, honoured in M6: a file
    /// written for the tunnel it needs should not have to wait.
    pub tunnel: Option<String>,
}

impl Profile {
    #[must_use]
    pub const fn kind(&self) -> DriverKind {
        match self.driver {
            DriverConfig::Postgres(_) => DriverKind::Postgres,
            DriverConfig::BigQuery(_) => DriverKind::BigQuery,
        }
    }

    /// Where the password comes from, if this driver has one.
    #[must_use]
    pub const fn secret(&self) -> Option<&SecretRef> {
        match &self.driver {
            DriverConfig::Postgres(pg) => pg.password.as_ref(),
            DriverConfig::BigQuery(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverConfig {
    Postgres(PostgresConfig),
    BigQuery(BigQueryConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub sslmode: SslMode,
    /// `None` means no password: a Unix socket, `.pgpass`, or trust.
    pub password: Option<SecretRef>,
}

impl PostgresConfig {
    pub const DEFAULT_PORT: u16 = 5432;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigQueryConfig {
    pub project: String,
    /// `None` lets the driver infer it from the dataset.
    pub location: Option<String>,
    pub auth: BigQueryAuth,
    /// Queries estimated above this are refused before they run.
    pub max_bytes_billed: Option<ByteSize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BigQueryAuth {
    /// Application default credentials — `gcloud auth application-default`.
    Adc,
    ServiceAccount(PathBuf),
}

/// Where a secret is, never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// The OS keyring: Keychain on macOS.
    Keyring,
    /// A command whose stdout is the secret, e.g. `op read op://...`.
    Command(String),
    /// An environment variable. The weakest of the three — it is visible to
    /// every child process — and offered because CI has nothing else.
    Env(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileColor {
    Red,
    Yellow,
    Green,
    Blue,
    Magenta,
    Cyan,
}

// ── the shape on disk ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectionsFile {
    #[serde(default)]
    pub(crate) connection: Vec<RawConnection>,
}

/// Every driver's keys in one table.
///
/// `deny_unknown_fields` is what makes a typo an error rather than a setting
/// that silently does nothing — `readonly = true` misspelled as `read_only` is
/// the difference between a safe connection and a live one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConnection {
    id: String,
    name: Option<String>,
    driver: String,
    readonly: Option<bool>,
    color: Option<ProfileColor>,
    tunnel: Option<String>,

    // postgres
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    sslmode: Option<String>,
    password: Option<RawSecret>,

    // bigquery
    project: Option<String>,
    location: Option<String>,
    auth: Option<RawAuth>,
    max_bytes_billed: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum RawSecret {
    Keyring(bool),
    Command(String),
    Env(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum RawAuth {
    Adc(bool),
    ServiceAccount(PathBuf),
}

impl RawConnection {
    pub(crate) fn validate(self, path: &Path) -> ConfigResult<Profile> {
        let id = ProfileId::parse(&self.id)
            .map_err(|why| ConfigError::invalid(path, format!("connection id: {why}")))?;

        let driver = match self.driver.as_str() {
            "postgres" => self.postgres(path, &id)?,
            "bigquery" => self.bigquery(path, &id)?,
            other => {
                return Err(ConfigError::invalid(
                    path,
                    format!(
                        "connection `{id}`: `{other}` is not a driver, try postgres or bigquery"
                    ),
                ));
            }
        };

        Ok(Profile {
            // `name = ""` is the blank tab the id fallback exists to prevent.
            name: self
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| id.as_str().to_owned()),
            id,
            driver,
            readonly: self.readonly.unwrap_or(false),
            color: self.color,
            tunnel: self.tunnel,
        })
    }

    fn postgres(&self, path: &Path, id: &ProfileId) -> ConfigResult<DriverConfig> {
        self.reject_foreign_keys(
            path,
            id,
            "postgres",
            "bigquery",
            &[
                ("project", self.project.is_some()),
                ("location", self.location.is_some()),
                ("auth", self.auth.is_some()),
                ("max_bytes_billed", self.max_bytes_billed.is_some()),
            ],
        )?;

        Ok(DriverConfig::Postgres(PostgresConfig {
            host: required(self.host.clone(), path, id, "host")?,
            port: match self.port {
                // Port 0 means "any free port" to a listener and nothing at all
                // to a client, so it is a typo every time it appears here.
                Some(0) => {
                    return Err(ConfigError::invalid(
                        path,
                        format!("connection `{id}`: `port = 0` is not a port"),
                    ));
                }
                Some(port) => port,
                None => PostgresConfig::DEFAULT_PORT,
            },
            database: required(self.database.clone(), path, id, "database")?,
            user: required(self.user.clone(), path, id, "user")?,
            sslmode: match &self.sslmode {
                // `SslMode::DEFAULT` is libpq's own, so a profile that says
                // nothing behaves the way `psql` with the same keys does.
                None => SslMode::DEFAULT,
                Some(text) => SslMode::parse(text).map_err(|why| {
                    ConfigError::invalid(path, format!("connection `{id}`: {why}"))
                })?,
            },
            password: self
                .password
                .as_ref()
                .map(|raw| raw.validate(path, id))
                .transpose()?,
        }))
    }

    fn bigquery(&self, path: &Path, id: &ProfileId) -> ConfigResult<DriverConfig> {
        self.reject_foreign_keys(
            path,
            id,
            "bigquery",
            "postgres",
            &[
                ("host", self.host.is_some()),
                ("port", self.port.is_some()),
                ("database", self.database.is_some()),
                ("user", self.user.is_some()),
                ("sslmode", self.sslmode.is_some()),
                ("password", self.password.is_some()),
            ],
        )?;

        let auth = match &self.auth {
            None => BigQueryAuth::Adc,
            Some(RawAuth::Adc(true)) => BigQueryAuth::Adc,
            Some(RawAuth::Adc(false)) => {
                return Err(ConfigError::invalid(
                    path,
                    format!(
                        "connection `{id}`: `auth = {{ adc = false }}` says nothing; \
                         name a service account, or remove the key"
                    ),
                ));
            }
            Some(RawAuth::ServiceAccount(file)) => {
                // Covers `~/keys/sa.json` and `keys/sa.json` alike: the first is
                // a shell expansion sqlake does not perform, the second resolves
                // against whichever directory sqlake happened to be started in.
                if !file.is_absolute() {
                    return Err(ConfigError::invalid(
                        path,
                        format!(
                            "connection `{id}`: write `service_account` as a full path — \
                             `~` is expanded by a shell and sqlake is not one, and a \
                             relative path depends on where sqlake was started"
                        ),
                    ));
                }
                BigQueryAuth::ServiceAccount(file.clone())
            }
        };

        if self
            .max_bytes_billed
            .is_some_and(|budget| budget.get() == 0)
        {
            return Err(ConfigError::invalid(
                path,
                format!(
                    "connection `{id}`: `max_bytes_billed = \"0\"` refuses every query; \
                     remove the key to leave the budget to the project"
                ),
            ));
        }

        Ok(DriverConfig::BigQuery(BigQueryConfig {
            project: required(self.project.clone(), path, id, "project")?,
            location: self.location.clone(),
            auth,
            max_bytes_billed: self.max_bytes_billed,
        }))
    }

    /// A key that belongs to the other driver is a mistake worth naming.
    ///
    /// Ignoring it would be worse than it sounds: `max_bytes_billed` on a
    /// profile that turned out to be `postgres` reads as a spending limit that
    /// is in force, and it is not.
    fn reject_foreign_keys(
        &self,
        path: &Path,
        id: &ProfileId,
        driver: &str,
        owner: &str,
        keys: &[(&str, bool)],
    ) -> ConfigResult<()> {
        let strays: Vec<&str> = keys
            .iter()
            .filter(|(_, present)| *present)
            .map(|(key, _)| *key)
            .collect();
        if strays.is_empty() {
            return Ok(());
        }
        Err(ConfigError::invalid(
            path,
            format!(
                "connection `{id}` is {driver}, but sets {} — {} {owner} {}",
                strays.join(", "),
                if strays.len() == 1 {
                    "that is a"
                } else {
                    "those are"
                },
                if strays.len() == 1 { "key" } else { "keys" },
            ),
        ))
    }
}

impl RawSecret {
    fn validate(&self, path: &Path, id: &ProfileId) -> ConfigResult<SecretRef> {
        match self {
            Self::Keyring(true) => Ok(SecretRef::Keyring),
            Self::Keyring(false) => Err(ConfigError::invalid(
                path,
                format!(
                    "connection `{id}`: `password = {{ keyring = false }}` says nothing; \
                     remove the key, or say where the password is"
                ),
            )),
            Self::Command(command) if command.trim().is_empty() => Err(ConfigError::invalid(
                path,
                format!("connection `{id}`: `password.command` is empty"),
            )),
            Self::Command(command) => Ok(SecretRef::Command(command.clone())),
            Self::Env(name) if name.trim().is_empty() => Err(ConfigError::invalid(
                path,
                format!("connection `{id}`: `password.env` is empty"),
            )),
            Self::Env(name) => Ok(SecretRef::Env(name.clone())),
        }
    }
}

/// A key that has to be there, and has to say something.
///
/// `host = ""` is a half-filled template, not a host called nothing: accepting
/// it trades this message for whatever the driver says when it fails to connect
/// to the empty string, which is a worse message about a worse problem.
fn required(value: Option<String>, path: &Path, id: &ProfileId, key: &str) -> ConfigResult<String> {
    match value {
        Some(text) if !text.trim().is_empty() => Ok(text),
        Some(_) => Err(ConfigError::invalid(
            path,
            format!("connection `{id}`: `{key}` is empty"),
        )),
        None => Err(ConfigError::invalid(
            path,
            format!("connection `{id}` needs `{key}`"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::connections_from_str;

    fn parse(toml: &str) -> ConfigResult<Vec<Profile>> {
        connections_from_str(toml, Path::new("connections.toml"))
    }

    fn one(toml: &str) -> Profile {
        parse(toml).expect("should parse").pop().expect("a profile")
    }

    fn why(toml: &str) -> String {
        parse(toml).expect_err("should not parse").to_string()
    }

    const PG: &str = r#"
        [[connection]]
        id = "prod-pg"
        driver = "postgres"
        host = "127.0.0.1"
        database = "app"
        user = "readonly"
    "#;

    #[test]
    fn a_minimal_postgres_profile_gets_the_libpq_defaults() {
        let profile = one(PG);
        assert_eq!(profile.kind(), DriverKind::Postgres);
        let DriverConfig::Postgres(pg) = &profile.driver else {
            panic!("expected postgres");
        };
        assert_eq!(pg.port, 5432);
        assert_eq!(pg.sslmode, SslMode::Prefer);
        assert_eq!(pg.password, None);
        // Unnamed profiles are shown by their id rather than as a blank tab.
        assert_eq!(profile.name, "prod-pg");
        assert!(!profile.readonly);
    }

    #[test]
    fn the_three_ways_to_say_where_a_password_is() {
        let secret = |line: &str| {
            one(&format!("{PG}\n{line}"))
                .secret()
                .expect("a secret")
                .clone()
        };
        assert_eq!(secret("password = { keyring = true }"), SecretRef::Keyring);
        assert_eq!(
            secret(r#"password = { command = "op read op://vault/db/password" }"#),
            SecretRef::Command("op read op://vault/db/password".into())
        );
        assert_eq!(
            secret(r#"password = { env = "PGPASSWORD" }"#),
            SecretRef::Env("PGPASSWORD".into())
        );
    }

    #[test]
    fn a_secret_that_says_nothing_is_refused() {
        // `keyring = false` parses, and would otherwise mean "no password" —
        // which is not what someone who typed the word keyring meant.
        let err = why(&format!("{PG}\npassword = {{ keyring = false }}"));
        assert!(err.contains("says nothing"), "{err}");
        assert!(err.contains("prod-pg"), "{err}");
    }

    #[test]
    fn a_key_belonging_to_the_other_driver_is_named() {
        // Silently ignoring this would present `max_bytes_billed` as a
        // spending limit that is in force. It is not.
        let err = why(&format!("{PG}\nmax_bytes_billed = \"20GB\""));
        assert!(err.contains("max_bytes_billed"), "{err}");
        assert!(err.contains("bigquery"), "{err}");
    }

    #[test]
    fn a_missing_key_names_the_connection_and_the_key() {
        let err = why(r#"
            [[connection]]
            id = "prod-pg"
            driver = "postgres"
            host = "127.0.0.1"
            user = "readonly"
        "#);
        assert!(err.contains("prod-pg"), "{err}");
        assert!(err.contains("database"), "{err}");
    }

    #[test]
    fn a_misspelled_key_is_an_error_not_a_shrug() {
        // `read_only` instead of `readonly` is the difference between a safe
        // connection and a live one, so it must not parse.
        let err = why(&format!("{PG}\nread_only = true"));
        assert!(err.contains("read_only"), "{err}");
    }

    #[test]
    fn an_unknown_driver_suggests_the_ones_that_exist() {
        let err = why(r#"
            [[connection]]
            id = "x"
            driver = "mysql"
        "#);
        assert!(err.contains("postgres"), "{err}");
        assert!(err.contains("bigquery"), "{err}");
    }

    #[test]
    fn bigquery_defaults_to_application_default_credentials() {
        let profile = one(r#"
            [[connection]]
            id = "bq"
            driver = "bigquery"
            project = "my-project"
        "#);
        assert_eq!(profile.kind(), DriverKind::BigQuery);
        let DriverConfig::BigQuery(bq) = &profile.driver else {
            panic!("expected bigquery");
        };
        assert_eq!(bq.auth, BigQueryAuth::Adc);
        assert_eq!(bq.location, None);
        assert_eq!(bq.max_bytes_billed, None);
        // A BigQuery profile has no password to look up anywhere.
        assert_eq!(profile.secret(), None);
    }

    #[test]
    fn a_service_account_path_is_not_expanded_by_sqlake() {
        let err = why(r#"
            [[connection]]
            id = "bq"
            driver = "bigquery"
            project = "p"
            auth = { service_account = "~/keys/sa.json" }
        "#);
        assert!(err.contains("full path"), "{err}");
    }

    #[test]
    fn an_id_has_to_survive_a_command_line_and_a_keyring_entry() {
        assert!(ProfileId::parse("prod-pg").is_ok());
        assert!(ProfileId::parse("").is_err());
        assert!(ProfileId::parse("prod pg").is_err());
        assert!(ProfileId::parse("prod/pg").is_err());
        // Dots are allowed, but an id of nothing else names a directory.
        assert!(ProfileId::parse("db.prod").is_ok());
        assert!(ProfileId::parse(".").is_err());
        assert!(ProfileId::parse("..").is_err());
    }

    #[test]
    fn a_key_that_is_present_but_empty_is_not_an_answer() {
        // `host = ""` otherwise reaches the driver, which reports a failure to
        // connect to nowhere instead of a config file that is half filled in.
        let err = why(r#"
            [[connection]]
            id = "prod-pg"
            driver = "postgres"
            host = ""
            database = "app"
            user = "readonly"
        "#);
        assert!(err.contains("host"), "{err}");
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn an_empty_name_falls_back_to_the_id() {
        // The same blank tab the id fallback exists to prevent.
        assert_eq!(one(&format!("{PG}\nname = \"\"")).name, "prod-pg");
    }

    #[test]
    fn port_zero_is_a_typo_not_a_port() {
        let err = why(&format!("{PG}\nport = 0"));
        assert!(err.contains("port"), "{err}");
    }

    #[test]
    fn a_relative_service_account_path_is_refused_too() {
        // It would resolve against whatever directory sqlake was started in.
        let err = why(r#"
            [[connection]]
            id = "bq"
            driver = "bigquery"
            project = "p"
            auth = { service_account = "keys/sa.json" }
        "#);
        assert!(err.contains("full path"), "{err}");
    }

    #[test]
    fn a_budget_of_zero_is_refused() {
        // It would refuse every query, which reads as sqlake being broken
        // rather than as the budget being the thing that is wrong.
        let err = why(r#"
            [[connection]]
            id = "bq"
            driver = "bigquery"
            project = "p"
            max_bytes_billed = "0"
        "#);
        assert!(err.contains("max_bytes_billed"), "{err}");
    }

    #[test]
    fn the_readable_name_is_kept_when_given() {
        let profile = one(&format!(
            "{PG}\nname = \"Prod (read replica)\"\ncolor = \"red\""
        ));
        assert_eq!(profile.name, "Prod (read replica)");
        assert_eq!(profile.color, Some(ProfileColor::Red));
    }
}
