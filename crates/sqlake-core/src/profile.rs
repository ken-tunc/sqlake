//! What a driver is handed in order to connect.
//!
//! A [`ResolvedProfile`] is the far side of the `Profile → ResolvedProfile`
//! conversion that `sqlake-config` performs: the secret has been read, and
//! nothing about a config file survives the trip. It lives here rather than
//! with the file format because **a driver depends on nothing but this crate**
//! — putting it in `sqlake-config` would make every driver depend on a
//! decision about TOML.
//!
//! [`Params`] gains a variant when a driver gains an implementation. BigQuery
//! arrives in M2; declaring it now would be a shape guessed months before
//! anything constructs one.

use std::fmt;

use thiserror::Error;

use crate::capability::DriverKind;
use crate::id::ProfileId;
use crate::secret::Secret;

/// The application layer holds one of these rather than reading files itself:
/// a profile can come from `sqlake-config`, from a test, or from a `--mock`
/// flag, and none of those belong in the store.
///
/// [`resolve`](Profiles::resolve) **blocks**. Reading a secret can talk to the
/// OS keyring, which can put a dialog on the user's screen and wait for a
/// fingerprint, so the caller runs it on a blocking task rather than this
/// trait pretending the wait does not exist.
pub trait Profiles: Send + Sync + fmt::Debug {
    fn list(&self) -> Vec<ProfileSummary>;

    fn resolve(&self, id: &ProfileId) -> Result<ResolvedProfile, ProfileError>;
}

/// Enough to show it, name it, and pick the driver it needs — all of which the
/// UI wants before a keyring dialog has been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: ProfileId,
    pub name: String,
    pub kind: DriverKind,
}

/// A string rather than a structured error: the causes live in
/// `sqlake-config`, which this crate must not know about, and the message is
/// already written for the person reading it.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct ProfileError(String);

impl ProfileError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The `Debug` derive is safe: the only secret it can hold is a [`Secret`],
/// which prints a placeholder.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub id: ProfileId,
    /// Ask the server to refuse writes. Enforcement belongs to the server —
    /// PostgreSQL sets `default_transaction_read_only` — because a client-side
    /// check protects nothing that a client-side bug cannot undo.
    pub readonly: bool,
    pub params: Params,
}

impl ResolvedProfile {
    #[must_use]
    pub const fn kind(&self) -> DriverKind {
        match self.params {
            Params::Postgres(_) => DriverKind::Postgres,
            Params::Mock => DriverKind::Mock,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Params {
    Postgres(PostgresParams),
    /// The in-memory driver, which has nothing to connect to. It still arrives
    /// as a profile, because the path a mock connection takes has to be the
    /// path a real one takes or it is not testing it.
    Mock,
}

#[derive(Debug, Clone)]
pub struct PostgresParams {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub sslmode: SslMode,
    /// `None` means the server is expected to want no password: a Unix socket,
    /// `.pgpass`, or trust.
    pub password: Option<Secret>,
}

/// libpq's `sslmode`, with libpq's meanings.
///
/// The names are the ones a user already knows from `psql` and from every
/// connection string they have written, so they are spelled the same way here.
/// There is no `Deserialize` on it: this crate has no opinion about file
/// formats, and the caller that reads one gets to say which key was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    /// libpq's default, so a profile that says nothing behaves the way `psql`
    /// with the same keys does.
    pub const DEFAULT: Self = Self::Prefer;

    /// Whether the server's certificate is checked at all.
    ///
    /// `require` encrypts and verifies nothing, which surprises people who
    /// read it as the strong setting: it stops a passive listener and not an
    /// active one.
    #[must_use]
    pub const fn verifies_certificate(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// The libpq spellings sqlake implements, and nothing else.
    ///
    /// The error lists them all, because the mode someone meant is always one
    /// of them and never worth guessing. libpq has a sixth — `allow`, which
    /// tries plaintext first — and it gets its own message: someone who copied
    /// it out of a working connection string must not be told it is not an
    /// sslmode, because it is.
    pub fn parse(text: &str) -> Result<Self, String> {
        Ok(match text {
            "disable" => Self::Disable,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            "allow" => {
                return Err("`allow` is a libpq sslmode sqlake does not implement; \
                            `prefer` negotiates the same two outcomes, TLS first"
                    .to_owned());
            }
            other => {
                return Err(format!(
                    "`{other}` is not an sslmode; \
                     it is one of disable, prefer, require, verify-ca, verify-full"
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(password: Option<&str>) -> ResolvedProfile {
        ResolvedProfile {
            id: ProfileId::parse("prod-pg").unwrap(),
            readonly: true,
            params: Params::Postgres(PostgresParams {
                host: "db.internal".to_owned(),
                port: 5432,
                database: "app".to_owned(),
                user: "readonly".to_owned(),
                sslmode: SslMode::VerifyFull,
                password: password.map(|p| Secret::new(p.to_owned())),
            }),
        }
    }

    #[test]
    fn debugging_a_whole_profile_leaks_nothing() {
        // This is the shape a connection failure gets logged in, so it is the
        // one that matters.
        let shown = format!("{:?}", profile(Some("hunter2")));
        assert!(shown.contains("db.internal"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn the_kind_comes_from_the_parameters() {
        assert_eq!(profile(None).kind(), DriverKind::Postgres);
    }

    #[test]
    fn only_the_verify_modes_check_a_certificate() {
        // `require` reads like the strong setting and is not one.
        assert!(!SslMode::Require.verifies_certificate());
        assert!(!SslMode::Prefer.verifies_certificate());
        assert!(SslMode::VerifyCa.verifies_certificate());
        assert!(SslMode::VerifyFull.verifies_certificate());
    }

    #[test]
    fn the_names_are_the_ones_libpq_uses() {
        assert_eq!(SslMode::VerifyFull.as_str(), "verify-full");
        assert_eq!(SslMode::DEFAULT, SslMode::Prefer);
        for mode in [
            SslMode::Disable,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(SslMode::parse(mode.as_str()), Ok(mode));
        }
    }

    #[test]
    fn libpqs_sixth_mode_is_refused_as_itself() {
        // `allow` is a real sslmode, so telling someone who copied it out of a
        // working connection string that it is not one would be a lie.
        let err = SslMode::parse("allow").unwrap_err();
        assert!(err.contains("does not implement"), "{err}");
        assert!(!err.contains("is not an sslmode"), "{err}");
    }

    #[test]
    fn a_mode_nobody_defined_lists_the_ones_that_exist() {
        // `verify_full` with an underscore is the likely typo, and guessing
        // that it meant `verify-full` would be guessing about TLS.
        let err = SslMode::parse("verify_full").unwrap_err();
        assert!(err.contains("verify-full"), "{err}");
        assert!(err.contains("disable"), "{err}");
    }
}
