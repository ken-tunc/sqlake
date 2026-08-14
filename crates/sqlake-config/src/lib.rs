//! Configuration: the files, the profiles in them, and where they live.
//!
//! Two files, both optional and both edited by hand:
//!
//! ```text
//! ~/.config/sqlake/config.toml        settings that are not about a connection
//! ~/.config/sqlake/connections.toml   connection profiles
//! ```
//!
//! A [`Profile`] holds no secret: it says *where* a password is — keyring, a
//! command, an environment variable. Reading it is [`resolve()`], which produces
//! a [`ResolvedProfile`](sqlake_core::profile::ResolvedProfile) and is the only
//! place in the workspace where a secret exists in plaintext. Exactly one
//! source is tried, the one the profile named.
//!
//! The crate sits below `sqlake-app` and above nothing: it knows what a
//! [`DriverKind`](sqlake_core::capability::DriverKind) is, and that is all the
//! database it knows.

pub mod bytes;
pub mod error;
mod load;
pub mod paths;
pub mod profile;
pub mod resolve;
pub mod settings;

pub use bytes::ByteSize;
pub use error::{ConfigError, ConfigResult};
pub use load::Config;
pub use profile::{
    BigQueryAuth, BigQueryConfig, DriverConfig, PostgresConfig, Profile, ProfileColor, SecretRef,
};
pub use resolve::{KEYRING_SERVICE, Keyring, OsKeyring, resolve, resolve_with};
pub use settings::Settings;
