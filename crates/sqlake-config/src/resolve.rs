//! `Profile → ResolvedProfile`: the one place a secret is read.
//!
//! A profile says *where* the password is. Reading it means talking to the OS
//! keyring, running a command, or looking at the environment — all three of
//! which block, and the first of which can put a dialog on the user's screen.
//! **Nothing here is async on purpose:** the caller runs it on a blocking task
//! rather than this crate pulling in a runtime to hide the fact.
//!
//! The keyring is behind a trait so that everything else can be tested for
//! real. A command and an environment variable can be exercised as themselves
//! in a test; a Keychain cannot, and faking all three would leave the two that
//! work unverified.

use std::process::Command;

use sqlake_core::id::ProfileId;
use sqlake_core::profile::{Params, PostgresParams, ResolvedProfile};
use sqlake_core::secret::Secret;
use zeroize::Zeroize as _;

use crate::error::{ConfigError, ConfigResult};
use crate::profile::{DriverConfig, Profile, SecretRef};

/// The service name every sqlake entry is stored under.
///
/// The account is the profile id, so `security find-generic-password -s sqlake
/// -a prod-pg` is how a human looks at the same entry.
pub const KEYRING_SERVICE: &str = "sqlake";

/// Where keyring lookups go.
///
/// Exists so tests do not need a Keychain — and so a headless CI runner, which
/// has no secret service at all, is not the thing that decides whether this
/// crate can be tested.
pub trait Keyring: std::fmt::Debug {
    fn password(&self, profile: &ProfileId) -> ConfigResult<Secret>;
}

/// The real one: the platform credential store.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsKeyring;

impl Keyring for OsKeyring {
    fn password(&self, profile: &ProfileId) -> ConfigResult<Secret> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, profile.as_str())
            .map_err(|err| secret_error(profile, format!("opening the keyring: {err}")))?;
        entry
            .get_password()
            .map(Secret::new)
            .map_err(|err| match err {
                keyring::Error::NoEntry => secret_error(profile, no_keyring_entry(profile)),
                other => secret_error(profile, format!("reading the keyring: {other}")),
            })
    }
}

/// Why there was nothing to read, and how to put an entry where
/// [`OsKeyring`] will look for it.
///
/// `security` is macOS' own tool and exists nowhere else, so every other
/// platform is told what to store rather than which command to type — a
/// command that does not exist is a worse answer than no command.
fn no_keyring_entry(profile: &ProfileId) -> String {
    if cfg!(target_os = "macos") {
        format!(
            "no keyring entry. Store one with: \
             security add-generic-password -s {KEYRING_SERVICE} -a {profile} -w"
        )
    } else {
        format!(
            "no keyring entry. Store one in the platform credential store \
             under service `{KEYRING_SERVICE}`, account `{profile}`"
        )
    }
}

/// Read the secret this profile names, using the platform keyring.
pub fn resolve(profile: &Profile) -> ConfigResult<ResolvedProfile> {
    resolve_with(profile, &OsKeyring)
}

/// Read the secret this profile names, from a keyring of the caller's choosing.
pub fn resolve_with(profile: &Profile, keyring: &dyn Keyring) -> ConfigResult<ResolvedProfile> {
    let params = match &profile.driver {
        DriverConfig::Postgres(pg) => {
            let password = pg
                .password
                .as_ref()
                .map(|secret| read(secret, &profile.id, keyring))
                .transpose()?;
            Params::Postgres(PostgresParams {
                host: pg.host.clone(),
                port: pg.port,
                database: pg.database.clone(),
                user: pg.user.clone(),
                sslmode: pg.sslmode,
                password,
            })
        }
        DriverConfig::BigQuery(_) => {
            return Err(secret_error(
                &profile.id,
                "BigQuery connections arrive in M2".to_owned(),
            ));
        }
    };

    Ok(ResolvedProfile {
        id: profile.id.clone(),
        readonly: profile.readonly,
        params,
    })
}

/// One source, the one the profile named. No fallback chain: a password that
/// silently comes from somewhere else is how the wrong database gets written
/// to.
fn read(secret: &SecretRef, profile: &ProfileId, keyring: &dyn Keyring) -> ConfigResult<Secret> {
    let secret = match secret {
        SecretRef::Keyring => keyring.password(profile)?,
        SecretRef::Command(command) => from_command(command, profile)?,
        SecretRef::Env(name) => from_env(name, profile)?,
    };
    if secret.is_empty() {
        return Err(secret_error(
            profile,
            "the password came back empty".to_owned(),
        ));
    }
    Ok(secret)
}

/// Run the command line through a shell and take its stdout.
///
/// A shell, because `op read op://vault/db/password | tr -d '\n'` is the kind
/// of thing people already have in their notes, and splitting the string on
/// spaces would break every quoted argument.
fn from_command(command: &str, profile: &ProfileId) -> ConfigResult<Secret> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|err| secret_error(profile, format!("running the password command: {err}")))?;

    // Whatever it printed is the password until proven otherwise, so every
    // path out of here wipes the buffer rather than dropping it: a helper that
    // prints the secret and *then* fails is the ordinary case, not an exotic
    // one, and `Secret` cannot protect bytes it never held.
    let mut stdout = output.stdout;

    if !output.status.success() {
        // Deliberately without the command's stderr. It is the obvious thing
        // to include and it is a hole in the one guarantee this module makes:
        // a helper that fails *after* printing the secret — or one that echoes
        // its input on the way out — would put the password in the log, and no
        // type can stop it once the bytes are in a message. The user owns the
        // command line, so they can run it and read the message themselves.
        stdout.zeroize();
        let status = output
            .status
            .code()
            .map_or_else(|| "a signal".to_owned(), |code| format!("status {code}"));
        return Err(secret_error(
            profile,
            format!("the password command exited with {status}; run it yourself to see why"),
        ));
    }

    let mut text = match String::from_utf8(stdout) {
        Ok(text) => text,
        Err(err) => {
            err.into_bytes().zeroize();
            return Err(secret_error(
                profile,
                "the password command printed bytes that are not text".to_owned(),
            ));
        }
    };
    // Only the line ending the command's own `println` added, and only one of
    // them — a password is allowed to begin or end with a space, and trimming
    // any further would silently produce a different password than the one
    // that is stored.
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(Secret::new(text))
}

fn from_env(name: &str, profile: &ProfileId) -> ConfigResult<Secret> {
    std::env::var(name)
        .map(Secret::new)
        .map_err(|_| secret_error(profile, format!("${name} is not set")))
}

fn secret_error(profile: &ProfileId, message: String) -> ConfigError {
    ConfigError::Secret {
        profile: profile.clone(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{BigQueryAuth, BigQueryConfig, PostgresConfig};
    use sqlake_core::profile::SslMode;

    #[derive(Debug)]
    struct FakeKeyring(Option<&'static str>);

    impl Keyring for FakeKeyring {
        fn password(&self, profile: &ProfileId) -> ConfigResult<Secret> {
            self.0
                .map(|p| Secret::new(p.to_owned()))
                .ok_or_else(|| secret_error(profile, "no keyring entry".to_owned()))
        }
    }

    fn profile_with(password: Option<SecretRef>) -> Profile {
        Profile {
            id: ProfileId::parse("prod-pg").unwrap(),
            name: "Prod".to_owned(),
            driver: DriverConfig::Postgres(PostgresConfig {
                host: "db.internal".to_owned(),
                port: 5432,
                database: "app".to_owned(),
                user: "readonly".to_owned(),
                sslmode: SslMode::VerifyFull,
                password,
            }),
            readonly: true,
            color: None,
            tunnel: None,
        }
    }

    fn resolved(profile: &Profile) -> ConfigResult<ResolvedProfile> {
        resolve_with(profile, &FakeKeyring(Some("from-the-keyring")))
    }

    fn password_of(resolved: &ResolvedProfile) -> Option<&str> {
        let Params::Postgres(pg) = &resolved.params;
        pg.password.as_ref().map(Secret::expose)
    }

    #[test]
    fn the_rest_of_the_profile_comes_across_unchanged() {
        let profile = profile_with(None);
        let resolved = resolved(&profile).expect("should resolve");
        assert_eq!(resolved.id, profile.id);
        assert!(resolved.readonly);
        let Params::Postgres(pg) = &resolved.params;
        assert_eq!(pg.host, "db.internal");
        assert_eq!(pg.sslmode, SslMode::VerifyFull);
        // No password key means no password, not an empty one.
        assert_eq!(pg.password.as_ref().map(Secret::expose), None);
    }

    #[test]
    fn a_keyring_password_is_read_from_the_keyring() {
        let resolved = resolved(&profile_with(Some(SecretRef::Keyring))).expect("should resolve");
        assert_eq!(password_of(&resolved), Some("from-the-keyring"));
    }

    #[test]
    fn a_command_password_is_its_stdout() {
        let profile = profile_with(Some(SecretRef::Command("printf hunter2".to_owned())));
        let resolved = resolved(&profile).expect("should resolve");
        assert_eq!(password_of(&resolved), Some("hunter2"));
    }

    #[test]
    fn only_the_trailing_newline_is_dropped() {
        // `echo` adds one. A password is allowed to start or end with a space,
        // and trimming both ends would quietly use a different password than
        // the one that is stored.
        let profile = profile_with(Some(SecretRef::Command("printf ' hunter2 \\n'".to_owned())));
        let spaces = resolved(&profile).expect("should resolve");
        assert_eq!(password_of(&spaces), Some(" hunter2 "));

        // One line ending, not every one: a second newline is part of the
        // password, and eating it is the same silent substitution.
        let profile = profile_with(Some(SecretRef::Command(
            "printf 'hunter2\\n\\n'".to_owned(),
        )));
        let two = resolved(&profile).expect("should resolve");
        assert_eq!(password_of(&two), Some("hunter2\n"));

        // A CRLF is one line ending too.
        let profile = profile_with(Some(SecretRef::Command(
            "printf 'hunter2\\r\\n'".to_owned(),
        )));
        let crlf = resolved(&profile).expect("should resolve");
        assert_eq!(password_of(&crlf), Some("hunter2"));
    }

    #[test]
    fn a_failing_command_reports_the_status_and_not_its_output() {
        let profile = profile_with(Some(SecretRef::Command(
            "echo 'vault is locked' >&2; exit 3".to_owned(),
        )));
        let err = resolved(&profile).unwrap_err().to_string();
        assert!(err.contains("status 3"), "{err}");
        assert!(err.contains("prod-pg"), "{err}");
        // Not the message it printed — see `from_command`.
        assert!(!err.contains("vault is locked"), "{err}");
    }

    #[test]
    fn a_command_that_prints_nothing_is_a_failure_not_a_password() {
        let profile = profile_with(Some(SecretRef::Command("true".to_owned())));
        let err = resolved(&profile).unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn an_environment_password_is_read_from_the_environment() {
        // `PATH` is set in every environment this test can run in, which is
        // what lets the real reader be tested rather than a stand-in.
        let profile = profile_with(Some(SecretRef::Env("PATH".to_owned())));
        let resolved = resolved(&profile).expect("should resolve");
        assert_eq!(
            password_of(&resolved),
            std::env::var("PATH").ok().as_deref()
        );
    }

    #[test]
    fn a_missing_variable_names_itself() {
        let profile = profile_with(Some(SecretRef::Env("SQLAKE_NO_SUCH_VAR".to_owned())));
        let err = resolved(&profile).unwrap_err().to_string();
        assert!(err.contains("SQLAKE_NO_SUCH_VAR"), "{err}");
    }

    #[test]
    fn one_source_is_tried_and_no_other() {
        // The keyring holds a password; this profile did not ask for it. A
        // fallback chain here is how a connection silently comes up with a
        // credential nobody pointed at.
        let profile = profile_with(Some(SecretRef::Env("SQLAKE_NO_SUCH_VAR".to_owned())));
        assert!(resolve_with(&profile, &FakeKeyring(Some("from-the-keyring"))).is_err());
    }

    #[test]
    fn a_failure_to_resolve_carries_no_secret_in_its_message() {
        // The error is what reaches the log and the status bar.
        let profile = profile_with(Some(SecretRef::Command(
            "echo hunter2 >&2; exit 1".to_owned(),
        )));
        let err = resolved(&profile).unwrap_err();
        assert!(!format!("{err:?}").contains("hunter2"), "{err:?}");
    }

    #[test]
    fn bigquery_says_which_milestone_it_arrives_in() {
        let profile = Profile {
            id: ProfileId::parse("bq").unwrap(),
            name: "bq".to_owned(),
            driver: DriverConfig::BigQuery(BigQueryConfig {
                project: "p".to_owned(),
                location: None,
                auth: BigQueryAuth::Adc,
                max_bytes_billed: None,
            }),
            readonly: false,
            color: None,
            tunnel: None,
        };
        let err = resolved(&profile).unwrap_err().to_string();
        assert!(err.contains("M2"), "{err}");
    }
}
