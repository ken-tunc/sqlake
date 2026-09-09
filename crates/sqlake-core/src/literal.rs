//! String literals, quoted for a dialect.
//!
//! The mirror of [`ident`](crate::ident), and it exists for the same reason:
//! [`QuotedLiteral`] has no public constructor, so assembly that accepts only
//! a `QuotedLiteral` cannot be handed something unquoted. Templates are where
//! that matters — a value substituted into a statement is text the client
//! wrote, and the quoting is the whole of what stops a value ending the
//! literal it was put in.

use std::fmt;

use crate::capability::Escaping;

/// A value on its way into a statement, as the person typed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Literal(String);

impl Literal {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Quote and escape for a specific dialect.
    ///
    /// A doubled quote is the standard escape and both dialects read it, so it
    /// is the only one used here. Where a backslash also escapes — BigQuery —
    /// the backslash itself has to be doubled first, or a value ending in one
    /// would escape the closing quote and run the literal on into the rest of
    /// the statement.
    #[must_use]
    pub fn quote(&self, escaping: Escaping) -> QuotedLiteral {
        let escaped = match escaping {
            Escaping::Backslash => self.0.replace('\\', "\\\\").replace('\'', "''"),
            Escaping::None => self.0.replace('\'', "''"),
        };
        QuotedLiteral(format!("'{escaped}'"))
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A literal that has been quoted and escaped for a specific dialect.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuotedLiteral(String);

impl QuotedLiteral {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuotedLiteral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(s: &str) -> String {
        Literal::new(s).quote(Escaping::None).as_str().to_owned()
    }

    fn bq(s: &str) -> String {
        Literal::new(s)
            .quote(Escaping::Backslash)
            .as_str()
            .to_owned()
    }

    #[test]
    fn a_plain_value_is_wrapped() {
        assert_eq!(pg("alice"), "'alice'");
        assert_eq!(bq("alice"), "'alice'");
    }

    #[test]
    fn a_quote_is_doubled_in_both_dialects() {
        assert_eq!(pg("o'brien"), "'o''brien'");
        assert_eq!(bq("o'brien"), "'o''brien'");
    }

    #[test]
    fn a_backslash_is_ordinary_where_the_server_says_so() {
        // PostgreSQL with `standard_conforming_strings` reads it as a
        // character. Doubling it there would change the value.
        assert_eq!(pg(r"C:\tmp"), r"'C:\tmp'");
        assert_eq!(bq(r"C:\tmp"), r"'C:\\tmp'");
    }

    #[test]
    fn a_value_cannot_end_the_literal_it_is_in() {
        // The whole point. A trailing backslash on a dialect that honours one
        // would otherwise escape the closing quote, and everything after it
        // would be read as part of the string — or, worse, as statement.
        let ending = bq(r"anything\");
        assert_eq!(ending, r"'anything\\'");

        let closing = pg("'; drop table users; --");
        assert_eq!(closing, "'''; drop table users; --'");
        // And what the scanner makes of it: one string and nothing after it.
        let statements = crate::sql::ValidatedSql::parse(
            &crate::sql::RawSql::new(format!("select {closing}")),
            Escaping::None,
        );
        assert!(statements.is_ok(), "{statements:?}");
    }
}
