//! Identifier quoting.
//!
//! [`QuotedIdent`] has no public constructor. The only way to obtain one is
//! [`Ident::quote`], so SQL assembly that accepts only `QuotedIdent` cannot be
//! handed something unquoted. Forgetting to quote becomes a compile error
//! rather than a query that breaks on the first upper-case table name.

use std::fmt;

use crate::capability::QuoteStyle;

/// A raw identifier, exactly as it came from the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ident(String);

impl Ident {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Quote and escape for a specific dialect.
    #[must_use]
    pub fn quote(&self, style: QuoteStyle) -> QuotedIdent {
        let quoted = match style {
            // Doubling the delimiter is the SQL standard escape.
            QuoteStyle::DoubleQuote => format!("\"{}\"", self.0.replace('"', "\"\"")),
            // BigQuery uses backslash escapes inside backticks, so the
            // backslash itself has to be escaped first.
            QuoteStyle::Backtick => {
                format!("`{}`", self.0.replace('\\', "\\\\").replace('`', "\\`"))
            }
        };
        QuotedIdent(quoted)
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An identifier that has been quoted and escaped for a specific dialect.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuotedIdent(String);

impl QuotedIdent {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Join a path into a qualified name, e.g. `"public"."users"`.
    #[must_use]
    pub fn join(parts: &[QuotedIdent]) -> String {
        parts
            .iter()
            .map(QuotedIdent::as_str)
            .collect::<Vec<_>>()
            .join(".")
    }
}

impl fmt::Display for QuotedIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dq(s: &str) -> String {
        Ident::new(s).quote(QuoteStyle::DoubleQuote).as_str().into()
    }

    fn bt(s: &str) -> String {
        Ident::new(s).quote(QuoteStyle::Backtick).as_str().into()
    }

    #[test]
    fn plain_identifiers_are_wrapped() {
        assert_eq!(dq("users"), r#""users""#);
        assert_eq!(bt("users"), "`users`");
    }

    #[test]
    fn case_is_preserved() {
        // The whole point of quoting: an unquoted MyTable folds to mytable in
        // PostgreSQL and stops resolving.
        assert_eq!(dq("MyTable"), r#""MyTable""#);
    }

    #[test]
    fn embedded_delimiters_are_escaped() {
        assert_eq!(dq(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(bt("we`ird"), r"`we\`ird`");
    }

    #[test]
    fn backslashes_are_escaped_before_backticks() {
        // Escaping in the other order would turn \` into \\` and change meaning.
        assert_eq!(bt(r"a\`b"), r"`a\\\`b`");
    }

    #[test]
    fn dots_do_not_split_an_identifier() {
        // A table literally named "a.b" is one identifier, not two.
        assert_eq!(dq("a.b"), r#""a.b""#);
    }

    #[test]
    fn empty_identifiers_round_trip_to_empty_quotes() {
        // Databases reject these; quoting is not the place to decide that.
        assert_eq!(dq(""), r#""""#);
        assert_eq!(bt(""), "``");
    }

    #[test]
    fn join_qualifies_a_path() {
        let parts = [
            Ident::new("public").quote(QuoteStyle::DoubleQuote),
            Ident::new("users").quote(QuoteStyle::DoubleQuote),
        ];
        assert_eq!(QuotedIdent::join(&parts), r#""public"."users""#);
    }
}
