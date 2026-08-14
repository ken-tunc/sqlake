//! Configuration: the files, the profiles in them, and where they live.
//!
//! Two files, both optional and both edited by hand:
//!
//! ```text
//! ~/.config/sqlake/config.toml        settings that are not about a connection
//! ~/.config/sqlake/connections.toml   connection profiles
//! ```
//!
//! Nothing here reads a secret. A [`Profile`] says *where* a password is —
//! keyring, a command, an environment variable — and resolving that is the
//! `Profile → ResolvedProfile` conversion, which is the only place a secret
//! exists in memory and the only place `zeroize` is needed.
//!
//! The crate sits below `sqlake-app` and above nothing: it knows what a
//! [`DriverKind`](sqlake_core::capability::DriverKind) is, and that is all the
//! database it knows.

pub mod bytes;
pub mod error;
mod load;
pub mod paths;
pub mod profile;
pub mod settings;

pub use bytes::ByteSize;
pub use error::{ConfigError, ConfigResult};
pub use load::Config;
pub use profile::{
    BigQueryAuth, BigQueryConfig, DriverConfig, PostgresConfig, Profile, ProfileColor, ProfileId,
    SecretRef, SslMode,
};
pub use settings::Settings;
