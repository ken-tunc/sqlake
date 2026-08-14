//! `config.toml`: what is true of sqlake rather than of one connection.
//!
//! Deliberately small. A setting that nothing reads is a promise the client
//! does not keep, so keys arrive with the milestone that honours them — the
//! editor and the cost thresholds with M4, the theme and the key map after
//! that.

use std::path::Path;

use serde::Deserialize;
use sqlake_core::result::PageRequest;

use crate::error::{ConfigError, ConfigResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Rows fetched per page, and per "load more".
    pub page_size: u32,
}

impl Settings {
    /// Big enough to fill a screen several times over, small enough that a
    /// mistyped table name does not pull a million rows.
    pub const MAX_PAGE_SIZE: u32 = 100_000;
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page_size: PageRequest::DEFAULT_LIMIT,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsFile {
    page_size: Option<u32>,
}

impl SettingsFile {
    pub(crate) fn validate(self, path: &Path) -> ConfigResult<Settings> {
        let defaults = Settings::default();
        let page_size = self.page_size.unwrap_or(defaults.page_size);
        match page_size {
            0 => Err(ConfigError::invalid(
                path,
                "`page_size = 0` would fetch nothing at all",
            )),
            size if size > Settings::MAX_PAGE_SIZE => Err(ConfigError::invalid(
                path,
                format!(
                    "`page_size = {size}` is more than {}; a page that large is a wait, not a page",
                    Settings::MAX_PAGE_SIZE
                ),
            )),
            page_size => Ok(Settings { page_size }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::settings_from_str;

    fn parse(toml: &str) -> ConfigResult<Settings> {
        settings_from_str(toml, Path::new("config.toml"))
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse("").unwrap(), Settings::default());
        assert_eq!(Settings::default().page_size, PageRequest::DEFAULT_LIMIT);
    }

    #[test]
    fn a_page_size_is_read() {
        assert_eq!(parse("page_size = 500").unwrap().page_size, 500);
    }

    #[test]
    fn the_two_page_sizes_that_are_not_pages_are_refused() {
        assert!(
            parse("page_size = 0")
                .unwrap_err()
                .to_string()
                .contains('0')
        );
        let err = parse("page_size = 1000000").unwrap_err().to_string();
        assert!(err.contains("100000"), "{err}");
    }

    #[test]
    fn a_misspelled_key_is_an_error() {
        // Otherwise `pagesize = 500` is a setting that appears to work.
        let err = parse("pagesize = 500").unwrap_err().to_string();
        assert!(err.contains("pagesize"), "{err}");
    }
}
