//! A string that must not be printed, logged, or left behind in memory.
//!
//! The guarantee is structural rather than procedural. `Secret` has no
//! `Display`, its `Debug` prints a placeholder, and the plaintext is reachable
//! only through [`Secret::expose`] — so "did this password reach the log?" is
//! answered by grepping for one function name instead of by reading every
//! format string in the workspace.

use std::fmt;

use zeroize::Zeroize as _;

/// A resolved secret: a password read from the keyring, a command or the
/// environment.
///
/// Dropping one overwrites the bytes it held. That is a best-effort measure,
/// not a proof: a `String` that reallocated while being built has already left
/// a copy behind, which is why the value is moved in whole from whatever
/// produced it and never assembled here.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub const fn new(secret: String) -> Self {
        Self(secret)
    }

    /// Read the plaintext.
    ///
    /// Every call site is a place a secret could escape, so there is exactly
    /// one name to search for.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    /// Never the value.
    ///
    /// `Secret` is a field of structs that derive `Debug`, so this is what
    /// stops a whole connection profile from becoming loggable by accident.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_nothing_of_the_secret() {
        let secret = Secret::new("hunter2".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(…)");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn a_struct_holding_one_is_safe_to_derive_debug_on() {
        // The point of the placeholder: this is how a secret actually reaches
        // a log — as a field of something else that someone printed.
        #[derive(Debug)]
        #[allow(dead_code, reason = "the fields exist to be printed by Debug")]
        struct Params {
            user: String,
            password: Secret,
        }

        let params = Params {
            user: "readonly".to_owned(),
            password: Secret::new("hunter2".to_owned()),
        };
        let shown = format!("{params:?}");
        assert!(shown.contains("readonly"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn the_plaintext_is_reachable_by_exactly_one_name() {
        let secret = Secret::new("hunter2".to_owned());
        assert_eq!(secret.expose(), "hunter2");
        assert!(!secret.is_empty());
        assert!(Secret::new(String::new()).is_empty());
    }
}
