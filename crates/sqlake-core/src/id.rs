//! Opaque identifiers.

use std::fmt;

use uuid::Uuid;

/// Identifies one open connection for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(Uuid);

impl ConnId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Short form for logs and tests. Not stable, not for display in the UI.
    #[must_use]
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_owned()
    }
}

impl Default for ConnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies one workspace tab. Allocated by the application layer, which is
/// why the inner counter is constructible here but never generated here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u32);

impl TabId {
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(n)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Names a connection profile — in the config file, on the command line, and
/// as the account of a keyring entry.
///
/// Restricted to characters that survive all three: an id with a space in it
/// works in TOML and then fails as an argument, and one with a slash would
/// build a keyring entry that is not the entry it looks like. It lives here
/// rather than in `sqlake-config` because a driver is handed one, and a driver
/// depends on nothing but this crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(String);

impl ProfileId {
    /// Letters, digits, `-`, `_` and `.`, and at least one that is not a dot.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("an id cannot be empty".into());
        }
        if let Some(bad) = text
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "`{text}` contains `{bad}`; ids are letters, digits, `-`, `_` and `.`"
            ));
        }
        // `.` and `..` pass the character test and then name a directory rather
        // than a connection anywhere an id becomes part of a path.
        if text.chars().all(|c| c == '.') {
            return Err(format!("`{text}` is not an id; it is a directory"));
        }
        Ok(Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_ids_are_distinct() {
        assert_ne!(ConnId::new(), ConnId::new());
    }

    #[test]
    fn short_form_is_eight_hex_digits() {
        let s = ConnId::new().short();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tab_id_round_trips() {
        assert_eq!(TabId::new(7).get(), 7);
        assert_eq!(TabId::new(7).to_string(), "#7");
    }

    #[test]
    fn a_profile_id_has_to_survive_a_command_line_and_a_keyring_entry() {
        assert_eq!(ProfileId::parse("prod-pg").unwrap().as_str(), "prod-pg");
        assert!(ProfileId::parse("db.prod").is_ok());
        assert!(ProfileId::parse("").is_err());
        assert!(ProfileId::parse("prod pg").is_err());
        assert!(ProfileId::parse("prod/pg").is_err());
        // Dots are allowed, but an id of nothing else names a directory.
        assert!(ProfileId::parse(".").is_err());
        assert!(ProfileId::parse("..").is_err());
    }
}
