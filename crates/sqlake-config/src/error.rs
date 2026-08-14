//! What can go wrong reading configuration.
//!
//! Every variant names the file. A message like "invalid type: string" with no
//! file and no key is the reason people give up on config files, and there are
//! two of them here.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub type ConfigResult<T> = Result<T, ConfigError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The file is not valid TOML, or does not match the shape expected.
    ///
    /// `toml`'s own message carries the line, the column and a caret, so it is
    /// printed as-is under the file name rather than summarised.
    #[error("{path}\n{source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The file parsed, but says something that cannot be acted on.
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },

    #[error("no home directory: set $HOME, or $XDG_CONFIG_HOME and $XDG_STATE_HOME")]
    NoHome,
}

impl ConfigError {
    pub(crate) fn invalid(path: &std::path::Path, message: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}
