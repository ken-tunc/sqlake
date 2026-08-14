//! Reading the two files, and what comes out of them.
//!
//! Both files are optional. A fresh install has neither, and refusing to start
//! without a config file would mean the first thing sqlake ever does is fail —
//! so a missing file is the defaults, and an unreadable one is an error.

use std::collections::HashSet;
use std::path::Path;

use crate::error::{ConfigError, ConfigResult};
use crate::paths;
use crate::profile::{ConnectionsFile, Profile};
use crate::settings::{Settings, SettingsFile};
use sqlake_core::id::ProfileId;

/// Everything sqlake was configured with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub settings: Settings,
    profiles: Vec<Profile>,
}

impl Config {
    /// Read `config.toml` and `connections.toml` from the standard directory.
    pub fn load() -> ConfigResult<Self> {
        Self::load_from(&paths::config_dir()?)
    }

    /// Read both files from `dir`, whether or not either exists.
    pub fn load_from(dir: &Path) -> ConfigResult<Self> {
        let settings_path = paths::settings_file(dir);
        let settings = match read_optional(&settings_path)? {
            Some(text) => settings_from_str(&text, &settings_path)?,
            None => Settings::default(),
        };

        let connections_path = paths::connections_file(dir);
        let profiles = match read_optional(&connections_path)? {
            Some(text) => connections_from_str(&text, &connections_path)?,
            None => Vec::new(),
        };

        Ok(Self { settings, profiles })
    }

    /// In file order, which is the order the user chose.
    #[must_use]
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    #[must_use]
    pub fn profile(&self, id: &ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|profile| &profile.id == id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// `Ok(None)` when the file is not there; an error for anything else, because
/// "unreadable" and "absent" are different problems and only one is normal.
fn read_optional(path: &Path) -> ConfigResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn settings_from_str(text: &str, path: &Path) -> ConfigResult<Settings> {
    toml::from_str::<SettingsFile>(text)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?
        .validate(path)
}

pub(crate) fn connections_from_str(text: &str, path: &Path) -> ConfigResult<Vec<Profile>> {
    let file: ConnectionsFile = toml::from_str(text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let profiles = file
        .connection
        .into_iter()
        .map(|raw| raw.validate(path))
        .collect::<ConfigResult<Vec<Profile>>>()?;

    let mut seen = HashSet::new();
    for profile in &profiles {
        if !seen.insert(&profile.id) {
            // Both would appear in the list, one would be unreachable by id,
            // and which one that is would depend on file order.
            return Err(ConfigError::invalid(
                path,
                format!("two connections are called `{}`", profile.id),
            ));
        }
    }

    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONNECTIONS: &str = r#"
        [[connection]]
        id = "prod-pg"
        driver = "postgres"
        host = "db.internal"
        database = "app"
        user = "readonly"

        [[connection]]
        id = "bq"
        driver = "bigquery"
        project = "my-project"
    "#;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).expect("write");
        }
        dir
    }

    #[test]
    fn a_directory_with_nothing_in_it_still_starts() {
        // A fresh install has no files at all, and the first thing sqlake does
        // must not be to fail.
        let dir = dir_with(&[]);
        let config = Config::load_from(dir.path()).expect("should load");
        assert_eq!(config.settings, Settings::default());
        assert!(config.is_empty());
    }

    #[test]
    fn both_files_are_read() {
        let dir = dir_with(&[
            ("config.toml", "page_size = 500"),
            ("connections.toml", CONNECTIONS),
        ]);
        let config = Config::load_from(dir.path()).expect("should load");
        assert_eq!(config.settings.page_size, 500);
        assert_eq!(config.profiles().len(), 2);
    }

    #[test]
    fn profiles_keep_the_order_they_were_written_in() {
        // The list is what the connection picker shows, and the order in the
        // file is the only preference anyone expressed.
        let dir = dir_with(&[("connections.toml", CONNECTIONS)]);
        let config = Config::load_from(dir.path()).expect("should load");
        let ids: Vec<&str> = config
            .profiles()
            .iter()
            .map(|profile| profile.id.as_str())
            .collect();
        assert_eq!(ids, ["prod-pg", "bq"]);
    }

    #[test]
    fn a_profile_is_found_by_id() {
        let dir = dir_with(&[("connections.toml", CONNECTIONS)]);
        let config = Config::load_from(dir.path()).expect("should load");
        let id = ProfileId::parse("bq").unwrap();
        assert_eq!(config.profile(&id).map(|p| p.name.as_str()), Some("bq"));
        assert!(config.profile(&ProfileId::parse("nope").unwrap()).is_none());
    }

    #[test]
    fn two_connections_with_one_id_are_refused() {
        // Otherwise one of them is unreachable by id, and which one depends on
        // where in the file it was written.
        let doubled = format!("{CONNECTIONS}\n{CONNECTIONS}");
        let err = connections_from_str(&doubled, Path::new("connections.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("prod-pg"), "{err}");
    }

    #[test]
    fn an_error_names_the_file_it_came_from() {
        let dir = dir_with(&[("connections.toml", "[[connection]]\nid = \"x\"\n")]);
        let err = Config::load_from(dir.path()).unwrap_err().to_string();
        assert!(err.contains("connections.toml"), "{err}");
    }

    #[test]
    fn a_directory_where_a_file_should_be_is_an_error_not_an_absence() {
        // `read_to_string` on a directory fails with something other than
        // NotFound, and reporting that as "no config" would hide it for ever.
        let dir = dir_with(&[]);
        std::fs::create_dir(dir.path().join("config.toml")).expect("mkdir");
        assert!(Config::load_from(dir.path()).is_err());
    }
}
