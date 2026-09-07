//! `config.toml`: what is true of sqlake rather than of one connection.
//!
//! Deliberately small. A setting that nothing reads is a promise the client
//! does not keep, so keys arrive with the milestone that honours them — the
//! editor and the cost thresholds with M4, the theme and the key map after
//! that.

use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;
use sqlake_core::result::PageRequest;

use crate::error::{ConfigError, ConfigResult};

/// The editor used when nothing says otherwise. POSIX requires it, so it is
/// the one name that is always there.
const FALLBACK_EDITOR: &str = "vi";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Rows fetched per page, and per "load more".
    pub page_size: u32,
    /// The program `e` hands the buffer to, overriding `$VISUAL` and
    /// `$EDITOR`.
    ///
    /// A program, not a command line: it is passed to `Command::new`
    /// unparsed, so an editor living under a path with a space in it works and
    /// nothing is word-split. Arguments go in `editor_args`, which is also the
    /// only place a shell would have been needed.
    pub editor: Option<String>,
    /// Arguments passed before the file name.
    ///
    /// Exists for GUI editors, which fork and return at once unless told to
    /// wait — `["--wait"]` for VS Code, `["-w"]` for BBEdit. Without one of
    /// those the client takes the terminal back before anything has been
    /// typed.
    pub editor_args: Vec<String>,
}

impl Settings {
    /// Big enough to fill a screen several times over, small enough that a
    /// mistyped table name does not pull a million rows.
    pub const MAX_PAGE_SIZE: u32 = 100_000;

    /// Which editor to launch, from the setting and the environment.
    ///
    /// Pure, and given the two variables rather than reading them, so the
    /// order is tested without a process-wide `set_var` — the same reason
    /// [`crate::paths`] splits its functions this way.
    ///
    /// `$VISUAL` before `$EDITOR` is the convention every tool that does this
    /// follows: `$EDITOR` is historically allowed to be a line editor, and
    /// `$VISUAL` is the one promised to work on a full screen.
    #[must_use]
    pub fn editor_program(&self, visual: Option<OsString>, editor: Option<OsString>) -> OsString {
        if let Some(configured) = &self.editor {
            return OsString::from(configured);
        }
        visual
            .into_iter()
            .chain(editor)
            .find(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from(FALLBACK_EDITOR))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page_size: PageRequest::DEFAULT_LIMIT,
            editor: None,
            editor_args: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsFile {
    page_size: Option<u32>,
    editor: Option<String>,
    editor_args: Option<Vec<String>>,
}

impl SettingsFile {
    pub(crate) fn validate(self, path: &Path) -> ConfigResult<Settings> {
        let defaults = Settings::default();
        if self.editor.as_ref().is_some_and(|e| e.trim().is_empty()) {
            return Err(ConfigError::invalid(
                path,
                "`editor` is empty, which names no program at all — remove it \
                 to fall back to $VISUAL, $EDITOR and then vi",
            ));
        }
        let editor = self.editor;
        let editor_args = self.editor_args.unwrap_or(defaults.editor_args.clone());
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
            page_size => Ok(Settings {
                page_size,
                editor,
                editor_args,
            }),
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
    fn the_editor_comes_from_the_setting_first() {
        let s = parse("editor = \"hx\"\neditor_args = [\"--vsplit\"]").unwrap();
        assert_eq!(
            s.editor_program(Some("vim".into()), Some("nano".into())),
            OsString::from("hx")
        );
        assert_eq!(s.editor_args, ["--vsplit"]);
    }

    #[test]
    fn visual_beats_editor_and_both_beat_vi() {
        // `$EDITOR` is historically allowed to be a line editor; `$VISUAL` is
        // the one promised to work on a full screen.
        let s = Settings::default();
        assert_eq!(
            s.editor_program(Some("vim".into()), Some("nano".into())),
            OsString::from("vim")
        );
        assert_eq!(
            s.editor_program(None, Some("nano".into())),
            OsString::from("nano")
        );
        assert_eq!(s.editor_program(None, None), OsString::from("vi"));
    }

    #[test]
    fn an_empty_variable_is_not_a_choice() {
        // `EDITOR=` in a shell profile is how a variable ends up set to
        // nothing, and launching "" fails with a message about no such file.
        let s = Settings::default();
        assert_eq!(
            s.editor_program(Some("".into()), Some("nano".into())),
            OsString::from("nano")
        );
        assert_eq!(
            s.editor_program(Some("".into()), None),
            OsString::from("vi")
        );
    }

    #[test]
    fn an_empty_editor_setting_is_refused_rather_than_ignored() {
        // Silently falling back would make `editor = ""` look like a setting
        // that works, and the reason the wrong editor opens impossible to see.
        let err = parse("editor = \"\"").unwrap_err().to_string();
        assert!(err.contains("editor"), "{err}");
    }

    #[test]
    fn a_misspelled_key_is_an_error() {
        // Otherwise `pagesize = 500` is a setting that appears to work.
        let err = parse("pagesize = 500").unwrap_err().to_string();
        assert!(err.contains("pagesize"), "{err}");
    }
}
