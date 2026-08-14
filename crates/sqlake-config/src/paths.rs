//! Where sqlake keeps its files.
//!
//! The XDG variables are honoured on every platform, including macOS. The
//! alternative is `~/Library/Application Support/sqlake`, which is where a Mac
//! GUI application belongs and not where anyone edits a config file by hand —
//! and this one is edited by hand.
//!
//! The functions that read the environment are thin wrappers around pure ones,
//! so the layout is tested without a process-wide `set_var`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;

const APP: &str = "sqlake";

/// `$XDG_CONFIG_HOME/sqlake`, or `~/.config/sqlake`.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    resolve(env("XDG_CONFIG_HOME"), env("HOME"), ".config").ok_or(ConfigError::NoHome)
}

/// `$XDG_STATE_HOME/sqlake`, or `~/.local/state/sqlake`.
///
/// Logs, scratch files and the SQLite database live here: things sqlake writes
/// for itself, which is exactly what the state directory is for.
pub fn state_dir() -> Result<PathBuf, ConfigError> {
    resolve(env("XDG_STATE_HOME"), env("HOME"), ".local/state").ok_or(ConfigError::NoHome)
}

/// Settings that are not about a particular connection.
#[must_use]
pub fn settings_file(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

/// Connection profiles.
#[must_use]
pub fn connections_file(config_dir: &Path) -> PathBuf {
    config_dir.join("connections.toml")
}

fn env(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// The XDG rule: an absolute `$XDG_*_HOME` wins, otherwise fall back under
/// `$HOME`.
///
/// A relative `$XDG_*_HOME` is ignored rather than joined onto the working
/// directory, which is what the specification asks for and also what stops a
/// stray `XDG_CONFIG_HOME=.` writing a config directory wherever sqlake
/// happens to be started.
fn resolve(xdg: Option<OsString>, home: Option<OsString>, under_home: &str) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(under_home)))?;
    Some(base.join(APP))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn an_absolute_xdg_directory_wins() {
        assert_eq!(
            resolve(os("/xdg"), os("/home/ken"), ".config"),
            Some(PathBuf::from("/xdg/sqlake"))
        );
    }

    #[test]
    fn home_is_the_fallback() {
        assert_eq!(
            resolve(None, os("/home/ken"), ".local/state"),
            Some(PathBuf::from("/home/ken/.local/state/sqlake"))
        );
    }

    #[test]
    fn a_relative_xdg_directory_is_ignored_rather_than_joined() {
        // `XDG_CONFIG_HOME=.` would otherwise put the config directory
        // wherever sqlake was started from, and a different one each time.
        assert_eq!(
            resolve(os("relative/path"), os("/home/ken"), ".config"),
            Some(PathBuf::from("/home/ken/.config/sqlake"))
        );
    }

    #[test]
    fn without_home_there_is_no_answer() {
        assert_eq!(resolve(None, None, ".config"), None);
    }

    #[test]
    fn the_file_names_hang_off_the_directory() {
        let dir = PathBuf::from("/xdg/sqlake");
        assert_eq!(
            settings_file(&dir),
            PathBuf::from("/xdg/sqlake/config.toml")
        );
        assert_eq!(
            connections_file(&dir),
            PathBuf::from("/xdg/sqlake/connections.toml")
        );
    }
}
