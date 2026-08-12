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
}
