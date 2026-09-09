//! `Template → BoundTemplate → RawSql`.
//!
//! A template is a statement with `{{placeholder}}` in it. Filling one in
//! produces text, and that text goes through [`ValidatedSql`](crate::sql::ValidatedSql) and
//! [`ApprovedQuery`](crate::sql::ApprovedQuery) like anything typed by hand —
//! a template is not a second way into the database, it is a faster way to the
//! same buffer.
//!
//! **Substituted, not bound.** Both servers take parameters, and using them
//! would look like the safer answer. It is the wrong one here:
//! `Session::execute` accepts only an `ApprovedQuery`, and an `ApprovedQuery`
//! requires an `Estimate` of the statement that runs. An estimate of
//! `WHERE day > ?` is not an estimate of the query with the date in it —
//! BigQuery prices a partitioned scan on the value. Substituting first keeps
//! the gate honest, and keeps the buffer readable: what is on screen is what
//! runs.
//!
//! Which makes the quoting the work rather than an afterthought. A value
//! becomes a [`QuotedLiteral`](crate::literal::QuotedLiteral) and an identifier a
//! [`QuotedIdent`](crate::ident::QuotedIdent), both by the rules of the
//! connection it is going to.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::capability::{Capabilities, Escaping, QuoteStyle};
use crate::ident::Ident;
use crate::literal::Literal;
use crate::sql::{InvalidSql, RawSql, Skipped, Token, scan};

/// The two dialect facts substitution needs.
///
/// Not `Capabilities` itself, which carries a hierarchy and a dozen flags that
/// have nothing to do with quoting — and which a test of this would then have
/// to build in order to ask about a comma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    pub quote_style: QuoteStyle,
    pub escaping: Escaping,
}

impl From<&Capabilities> for Dialect {
    fn from(capabilities: &Capabilities) -> Self {
        Self {
            quote_style: capabilities.quote_style,
            escaping: capabilities.escaping,
        }
    }
}

/// What a placeholder is going to be in the statement.
///
/// Two, and not a third for pasting text in unchanged. A raw placeholder would
/// be the one hole in everything above, and somebody who needs one can edit
/// the statement — which is a thing this client is built around rather than a
/// workaround.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `{{name}}` — a value, quoted as a string literal.
    ///
    /// The default because it is the safe one: an identifier pasted where a
    /// value belongs is a syntax error, and a value pasted where an identifier
    /// belongs is how somebody's `orders` table becomes somebody's problem.
    Value,
    /// `{{ident:name}}` — a table, a column, a schema.
    Ident,
}

impl Kind {
    /// `value:` is accepted as well as being the default: somebody who writes
    /// the kind out in full is saying what they mean, and refusing that would
    /// teach them the prefix does not exist.
    fn named(prefix: &str) -> Option<Self> {
        match prefix {
            "ident" => Some(Self::Ident),
            "value" => Some(Self::Value),
            _ => None,
        }
    }
}

/// One `{{…}}` in a template body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    pub name: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    /// The body is not something the scanner can walk — an unterminated
    /// string, say. Reported as itself rather than as a template problem,
    /// because it is the same failure the buffer would give.
    #[error(transparent)]
    Body(#[from] InvalidSql),

    #[error("a `{{{{` with no `}}}}` after it")]
    Unterminated,

    #[error("`{{{{{found}}}}}` has no name in it")]
    Nameless { found: String },

    /// A name with something other than letters, digits and `_` in it.
    ///
    /// Refused rather than accepted, so that `{{ table }}` is an error a
    /// person can see instead of a placeholder whose name has spaces in it
    /// and which nothing they type will ever match.
    #[error("`{name}` is not a placeholder name")]
    BadName { name: String },

    /// `{{ident:x}}` is the only prefix there is, so anything else is a typo —
    /// and a typo read as part of the name would silently make an identifier
    /// into a value.
    #[error("`{kind}:` is not a kind of placeholder")]
    UnknownKind { kind: String },

    /// The value is quoted on the way in, so a placeholder that is already
    /// inside quotes would end up quoted twice.
    #[error(
        "`{{{{{name}}}}}` is inside a string — the value is quoted for you, so take the quotes off"
    )]
    InLiteral { name: String },

    /// One name, quoted two ways.
    ///
    /// Refused rather than resolved, because there is nothing to resolve it
    /// to: the pane asks for `col` once, and the statement would then put the
    /// one answer in as a table name in one place and a string in another.
    /// Whichever the author meant, one of the two is wrong.
    #[error("`{name}` is a value in one place and an identifier in another")]
    TwoKinds { name: String },

    #[error("`{name}` has no value")]
    Missing { name: String },

    /// A value handed in for a placeholder the body does not have. A caller
    /// that misspelled one would otherwise be told nothing and get a statement
    /// still asking for the real one.
    #[error("there is no `{{{{{name}}}}}` in this template")]
    Unwanted { name: String },
}

/// Every placeholder in the body, in the order they first appear.
///
/// Deduplicated by name: `{{table}}` twice is one thing to ask for, and asking
/// twice would let somebody answer differently each time.
pub fn placeholders(body: &str, dialect: Dialect) -> Result<Vec<Placeholder>, TemplateError> {
    let mut found: Vec<Placeholder> = Vec::new();
    for (placeholder, _) in parse(body, dialect)? {
        if !found.iter().any(|held| held.name == placeholder.name) {
            found.push(placeholder);
        }
    }
    Ok(found)
}

/// A template with every placeholder filled in.
///
/// The stage design.md §4.1 reserves. It exists so that a template with an
/// unanswered placeholder cannot reach the buffer: the only way to a
/// `BoundTemplate` is [`BoundTemplate::bind`], which refuses one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundTemplate(String);

impl BoundTemplate {
    /// Fill in every placeholder, quoting each by what it is.
    ///
    /// `values` are what a person typed, uninterpreted: a value is a value,
    /// not an expression, so `now()` becomes the four characters `now()` in a
    /// string. Somebody who wants the function writes it in the template.
    pub fn bind(
        body: &str,
        values: &BTreeMap<String, String>,
        dialect: Dialect,
    ) -> Result<Self, TemplateError> {
        let found = parse(body, dialect)?;

        if let Some(name) = values
            .keys()
            .find(|given| !found.iter().any(|(p, _)| &&p.name == given))
        {
            return Err(TemplateError::Unwanted { name: name.clone() });
        }

        let mut out = String::with_capacity(body.len());
        let mut at = 0;
        for (placeholder, (start, end)) in found {
            let value = values
                .get(&placeholder.name)
                .ok_or_else(|| TemplateError::Missing {
                    name: placeholder.name.clone(),
                })?;
            out.push_str(&body[at..start]);
            let quoted = match placeholder.kind {
                Kind::Value => Literal::new(value.clone())
                    .quote(dialect.escaping)
                    .as_str()
                    .to_owned(),
                Kind::Ident => Ident::new(value.clone())
                    .quote(dialect.quote_style)
                    .as_str()
                    .to_owned(),
            };
            out.push_str(&quoted);
            at = end;
        }
        out.push_str(&body[at..]);
        Ok(Self(out))
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }
}

impl From<BoundTemplate> for RawSql {
    fn from(bound: BoundTemplate) -> Self {
        Self::new(bound.0)
    }
}

/// A placeholder and the bytes it occupies in the body.
type Found = (Placeholder, (usize, usize));

/// Every placeholder that will be substituted, with the span it occupies.
///
/// One inside a comment is not one: it is invisible to the server, so
/// substituting there would ask somebody for a value nothing reads, and
/// refusing would stop them writing `-- see {{table}}`. One inside a string
/// *is* refused, which is [`TemplateError::InLiteral`].
fn parse(body: &str, dialect: Dialect) -> Result<Vec<Found>, TemplateError> {
    let mut skipped = Vec::new();
    scan(body, dialect.escaping, |token| {
        if let Token::Skipped { start, end, what } = token {
            skipped.push((start, end, what));
        }
    })?;
    let inside = |at: usize| {
        skipped
            .iter()
            .find(|(start, end, _)| at >= *start && at < *end)
            .map(|(_, _, what)| *what)
    };

    let mut found = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(start) = find(bytes, b"{{", i) {
        let Some(end) = find(bytes, b"}}", start + 2) else {
            return Err(TemplateError::Unterminated);
        };
        let end = end + 2;
        let inner = &body[start + 2..end - 2];
        i = end;

        match inside(start) {
            // Invisible to the server, so there is nothing to fill in.
            Some(Skipped::Comment) => continue,
            Some(Skipped::Quoted) => {
                return Err(TemplateError::InLiteral {
                    name: inner.to_owned(),
                });
            }
            None => {}
        }
        let placeholder = placeholder(inner)?;
        if let Some((held, _)) = found
            .iter()
            .find(|(held, _): &&Found| held.name == placeholder.name)
            && held.kind != placeholder.kind
        {
            return Err(TemplateError::TwoKinds {
                name: placeholder.name,
            });
        }
        found.push((placeholder, (start, end)));
    }
    Ok(found)
}

fn placeholder(inner: &str) -> Result<Placeholder, TemplateError> {
    let (kind, name) = match inner.split_once(':') {
        Some((prefix, name)) => (
            Kind::named(prefix).ok_or_else(|| TemplateError::UnknownKind {
                kind: prefix.to_owned(),
            })?,
            name,
        ),
        None => (Kind::Value, inner),
    };
    if name.is_empty() {
        return Err(TemplateError::Nameless {
            found: inner.to_owned(),
        });
    }
    if !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Err(TemplateError::BadName {
            name: name.to_owned(),
        });
    }
    Ok(Placeholder {
        name: name.to_owned(),
        kind,
    })
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Dialect = Dialect {
        quote_style: QuoteStyle::DoubleQuote,
        escaping: Escaping::None,
    };
    const BQ: Dialect = Dialect {
        quote_style: QuoteStyle::Backtick,
        escaping: Escaping::Backslash,
    };

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn bound(body: &str, pairs: &[(&str, &str)], dialect: Dialect) -> String {
        BoundTemplate::bind(body, &values(pairs), dialect)
            .expect("it binds")
            .text()
            .to_owned()
    }

    #[test]
    fn a_value_is_quoted_and_an_identifier_is_not_quoted_the_same_way() {
        assert_eq!(
            bound(
                "select * from {{ident:table}} where name = {{name}}",
                &[("table", "users"), ("name", "alice")],
                PG
            ),
            r#"select * from "users" where name = 'alice'"#
        );
        assert_eq!(
            bound(
                "select * from {{ident:table}} where name = {{name}}",
                &[("table", "users"), ("name", "alice")],
                BQ
            ),
            "select * from `users` where name = 'alice'"
        );
    }

    #[test]
    fn a_value_cannot_end_the_statement_it_is_in() {
        // The reason substitution is allowed at all: the quoting is not
        // optional, and there is no way to reach `BoundTemplate` around it.
        let text = bound(
            "select * from t where name = {{name}}",
            &[("name", "'; drop table users; --")],
            PG,
        );
        assert_eq!(
            text,
            "select * from t where name = '''; drop table users; --'"
        );
        // And the scanner agrees it is one statement.
        let parsed = crate::sql::ValidatedSql::parse(&RawSql::new(text), Escaping::None);
        assert!(parsed.is_ok(), "{parsed:?}");
    }

    #[test]
    fn an_identifier_cannot_either() {
        let text = bound(
            "select * from {{ident:table}}",
            &[("table", r#"users" ; drop table x; --"#)],
            PG,
        );
        assert_eq!(text, r#"select * from "users"" ; drop table x; --""#);
        let parsed = crate::sql::ValidatedSql::parse(&RawSql::new(text), Escaping::None);
        assert!(parsed.is_ok(), "{parsed:?}");
    }

    #[test]
    fn a_value_is_a_value_and_not_an_expression() {
        assert_eq!(
            bound("select {{when}}", &[("when", "now()")], PG),
            "select 'now()'"
        );
    }

    #[test]
    fn the_same_placeholder_twice_is_asked_for_once_and_filled_in_both_times() {
        let body = "select {{ident:col}} from t order by {{ident:col}}";
        assert_eq!(
            placeholders(body, PG).expect("they parse"),
            [Placeholder {
                name: "col".to_owned(),
                kind: Kind::Ident
            }]
        );
        assert_eq!(
            bound(body, &[("col", "created_at")], PG),
            r#"select "created_at" from t order by "created_at""#
        );
    }

    #[test]
    fn one_name_cannot_be_two_things() {
        // Asked for once and filled in twice, so one of the two answers would
        // be quoted the way the other needed.
        let refused = placeholders("select {{col}} from t order by {{ident:col}}", PG)
            .expect_err("it should refuse");
        assert_eq!(
            refused,
            TemplateError::TwoKinds {
                name: "col".to_owned()
            }
        );
    }

    #[test]
    fn the_kind_can_be_written_out_in_full() {
        assert_eq!(bound("select {{value:a}}", &[("a", "1")], PG), "select '1'");
    }

    #[test]
    fn placeholders_come_back_in_the_order_they_are_read_in() {
        let found = placeholders("select {{b}}, {{a}}, {{b}} from t", PG).expect("they parse");
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[test]
    fn a_placeholder_with_no_value_stops_it_becoming_sql() {
        // The whole reason the stage exists: an unfilled placeholder reaching
        // the buffer is a syntax error reported against text nobody wrote.
        let refused = BoundTemplate::bind("select {{a}}, {{b}}", &values(&[("a", "1")]), PG)
            .expect_err("it should refuse");
        assert_eq!(
            refused,
            TemplateError::Missing {
                name: "b".to_owned()
            }
        );
    }

    #[test]
    fn a_value_for_something_that_is_not_there_is_refused() {
        let refused = BoundTemplate::bind("select {{a}}", &values(&[("a", "1"), ("c", "2")]), PG)
            .expect_err("it should refuse");
        assert_eq!(
            refused,
            TemplateError::Unwanted {
                name: "c".to_owned()
            }
        );
    }

    #[test]
    fn a_placeholder_inside_a_string_says_what_to_do_about_it() {
        // `'{{name}}'` would become `''alice''`, which is an empty string next
        // to a word. Refusing says why; substituting would produce a syntax
        // error pointing at the template's own quotes.
        let refused = BoundTemplate::bind(
            "select * from t where name = '{{name}}'",
            &values(&[("name", "alice")]),
            PG,
        )
        .expect_err("it should refuse");
        assert_eq!(
            refused,
            TemplateError::InLiteral {
                name: "name".to_owned()
            }
        );
        assert!(refused.to_string().contains("take the quotes off"));
    }

    #[test]
    fn a_placeholder_in_a_comment_is_left_where_it_is() {
        // Nothing reads it, so asking for a value would be asking for nothing.
        let body = "-- fill in {{ident:table}} yourself\nselect 1";
        assert!(placeholders(body, PG).expect("it parses").is_empty());
        assert_eq!(bound(body, &[], PG), body);

        let block = "/* {{a}} */ select {{b}}";
        assert_eq!(
            placeholders(block, PG)
                .expect("it parses")
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[test]
    fn a_dollar_quoted_body_is_a_string_too() {
        // PostgreSQL's other way of writing one, and the scanner already knows
        // it — which is why this reuses that walk rather than looking for
        // quotes itself.
        let refused = BoundTemplate::bind(
            "create function f() returns int as $$ select {{n}} $$ language sql",
            &values(&[("n", "1")]),
            PG,
        )
        .expect_err("it should refuse");
        assert!(
            matches!(refused, TemplateError::InLiteral { .. }),
            "{refused:?}"
        );
    }

    #[test]
    fn a_kind_nobody_defined_is_a_typo_rather_than_a_name() {
        // `{{indent:table}}` read as a value would quote a table name as a
        // string, and the error would be about the query rather than the typo.
        let refused = placeholders("select {{indent:table}}", PG).expect_err("it should refuse");
        assert_eq!(
            refused,
            TemplateError::UnknownKind {
                kind: "indent".to_owned()
            }
        );
    }

    #[test]
    fn a_name_that_cannot_be_typed_back_is_refused() {
        assert_eq!(
            placeholders("select {{ table }}", PG).expect_err("refused"),
            TemplateError::BadName {
                name: " table ".to_owned()
            }
        );
        assert_eq!(
            placeholders("select {{}}", PG).expect_err("refused"),
            TemplateError::Nameless {
                found: String::new()
            }
        );
        assert_eq!(
            placeholders("select {{ident:}}", PG).expect_err("refused"),
            TemplateError::Nameless {
                found: "ident:".to_owned()
            }
        );
    }

    #[test]
    fn an_opening_with_no_closing_is_refused() {
        assert_eq!(
            placeholders("select {{a", PG).expect_err("refused"),
            TemplateError::Unterminated
        );
    }

    #[test]
    fn a_body_the_scanner_cannot_walk_says_so_in_its_own_words() {
        // Not a template failure — the same one the buffer would report.
        let refused = placeholders("select 'unterminated {{a}}", PG).expect_err("refused");
        assert_eq!(
            refused,
            TemplateError::Body(InvalidSql::Unterminated("string"))
        );
    }

    #[test]
    fn a_template_with_no_placeholders_is_itself() {
        assert!(placeholders("select 1", PG).expect("it parses").is_empty());
        assert_eq!(bound("select 1", &[], PG), "select 1");
    }

    #[test]
    fn what_comes_out_is_what_goes_to_the_gate() {
        // A `BoundTemplate` is not a statement anybody may run: it becomes a
        // `RawSql` and takes the same path everything typed takes.
        let bound = BoundTemplate::bind("select {{a}}", &values(&[("a", "1")]), PG).expect("bound");
        let raw = RawSql::from(bound);
        assert_eq!(raw.text(), "select '1'");
    }
}
