//! SQL on its way to a server, as a sequence of types.
//!
//! `RawSql` is what somebody typed. [`ValidatedSql`] is one statement rather
//! than several. [`ApprovedQuery`] is one that has been costed, and it is the
//! only thing [`Session::execute`](crate::driver::Session::execute) accepts —
//! so **there is no code path that runs a query without estimating it first**.
//! The BigQuery billing accident is prevented by the type, not by remembering.
//!
//! Here rather than in `sqlake-app` — where design.md puts the staged types —
//! because the guarantee is the trait signature, and a trait in this crate
//! cannot name a type from the crate above it. `Ident → QuotedIdent` is here
//! for the same reason.
//!
//! Two stages, not the three design.md §4.1 lists. `PreparedSql` was to carry
//! bound parameters, and there are none until templates arrive in M7; between
//! [`ValidatedSql`] and here it would hold nothing, and §15 says to add a stage
//! only when skipping it would cause a real accident.

use std::fmt;

use thiserror::Error;

use crate::capability::Escaping;

/// What somebody typed, believed about not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSql(String);

impl RawSql {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidSql {
    #[error("there is nothing to run")]
    Empty,

    /// Refused rather than run. PostgreSQL's simple query protocol will happily
    /// run `DROP TABLE x; SELECT 1` in one round trip and report success, and a
    /// client whose grid can show only the last result is one that hides what
    /// it did.
    #[error("{count} statements — run them one at a time")]
    Several { count: usize },

    /// A quote, a comment or a dollar tag that never ends. The server would
    /// refuse it too, but only after being sent something whose extent nothing
    /// here could work out — which is also what makes the statement count
    /// meaningless.
    #[error("unterminated {0}")]
    Unterminated(&'static str),
}

/// Exactly one statement, and nothing that runs off the end of the text.
///
/// The only way to make one is [`ValidatedSql::parse`], so the invariant cannot
/// be asserted into existence somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSql {
    text: String,
    /// Where in the text this was parsed out of the statement begins.
    ///
    /// Kept because [`parse`](ValidatedSql::parse) drops the whitespace before
    /// it: a server counts an error position from the statement it was sent,
    /// and a buffer that opens with a blank line would have every one of them
    /// marked a line above where it belongs.
    start: Position,
}

impl ValidatedSql {
    /// Check that `raw` is one statement, reading literals the way a server
    /// with this [`Escaping`] does.
    ///
    /// Not a `TryFrom`: where a string literal ends depends on which server
    /// this is for, and a conversion with nowhere to say would have to guess —
    /// in a direction that is wrong for somebody whichever way it went.
    ///
    /// # Errors
    ///
    /// [`InvalidSql`], naming which of the three things it is.
    pub fn parse(raw: &RawSql, escaping: Escaping) -> Result<Self, InvalidSql> {
        let text = raw.text();
        let spans = statement_spans(text, escaping)?;
        match spans.len() {
            0 => Err(InvalidSql::Empty),
            // Surrounding whitespace and a trailing `;` go: what is kept is the
            // statement, so `select 1;` and `select 1` are the same query and
            // an error position counted from the start still lands.
            1 => {
                let (start, end) = spans[0];
                let span = &text[start..end];
                let kept = span.trim_start();
                let before = &text[..start + (span.len() - kept.len())];
                Ok(Self {
                    text: kept.trim_end().to_owned(),
                    start: Position::after_str(before),
                })
            }
            count => Err(InvalidSql::Several { count }),
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// A position the server counted from this statement, as a position in the
    /// text it was parsed out of.
    ///
    /// Applied here rather than in a front-end because this is what knows how
    /// much was dropped; a driver only ever sees the statement.
    #[must_use]
    pub const fn in_source(&self, at: Position) -> Position {
        if at.line == 1 {
            Position::new(
                self.start.line,
                self.start
                    .column
                    .saturating_add(at.column.saturating_sub(1)),
            )
        } else {
            Position::new(
                self.start.line.saturating_add(at.line.saturating_sub(1)),
                at.column,
            )
        }
    }
}

impl fmt::Display for ValidatedSql {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Where in a statement something is, one-based in lines and columns.
///
/// Lines and columns rather than the offset a server happens to report,
/// because converting is the one step that needs to know which server said it:
/// PostgreSQL gives a character offset into the whole statement and BigQuery
/// writes `[3:15]` into the message. Doing it in the driver is the rule
/// [`Capabilities`](crate::capability::Capabilities) follows, applied to
/// errors — it is what lets a front-end mark a line without asking who
/// reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl Position {
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// From a one-based character offset into `text`, which is how PostgreSQL
    /// reports it.
    ///
    /// `None` for an offset past the end or of zero: the server says zero for
    /// "no position", and pointing at a character that is not there would put
    /// a marker on a line the user cannot see.
    #[must_use]
    pub fn of_offset(text: &str, offset: u32) -> Option<Self> {
        let offset = usize::try_from(offset).ok()?;
        let mut at = Self::new(1, 1);
        for (seen, ch) in text.chars().enumerate() {
            // Checked before consuming the character, so an offset one past
            // the last one falls out of the loop rather than landing on it.
            if seen + 1 == offset {
                return Some(at);
            }
            at = at.after(ch);
        }
        None
    }

    /// Where the character after `text` would be.
    #[must_use]
    fn after_str(text: &str) -> Self {
        text.chars().fold(Self::new(1, 1), Self::after)
    }

    #[must_use]
    const fn after(self, ch: char) -> Self {
        if ch == '\n' {
            Self::new(self.line.saturating_add(1), 1)
        } else {
            Self::new(self.line, self.column.saturating_add(1))
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// What a query is expected to cost, in whatever the server actually measures.
///
/// An enum rather than a number because the two drivers do not measure the
/// same thing: BigQuery's dry run gives bytes that turn into money, and
/// PostgreSQL's `EXPLAIN` gives planner cost units that do not. Only the first
/// can be compared to a budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Estimate {
    /// Bytes the query will be billed for.
    Bytes(u64),
    /// The planner's own units. Comparable between two plans on one server and
    /// meaningless anywhere else, so it is shown and never gates.
    Cost(f64),
    /// The driver does not estimate. [`Capabilities::cost_estimate`] says so
    /// in advance, so this is expected rather than a failure.
    ///
    /// [`Capabilities::cost_estimate`]: crate::capability::Capabilities::cost_estimate
    Unknown,
}

impl Estimate {
    /// The bytes, when that is what was measured.
    #[must_use]
    pub const fn bytes(self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

/// A query that was estimated and then allowed.
///
/// The estimate is a field rather than a promise: constructing one without
/// having asked the server is not possible, which is the whole guarantee.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovedQuery {
    sql: ValidatedSql,
    /// Rows to fetch, or all of them.
    ///
    /// Applied to the fetch and never to the text. Appending a `LIMIT` would
    /// change the statement the user wrote and the offsets its errors are
    /// reported at, and it is wrong outright for a CTE ending in
    /// `INSERT … RETURNING`. Both servers take a row cap on the fetch itself.
    max_rows: Option<u32>,
    estimate: Estimate,
    granted: Approval,
}

/// Why a query was allowed to run.
///
/// Kept so the reason can be shown and logged. "Nobody could measure it" and
/// "somebody said yes to a number" are different events, and a client that
/// recorded them the same way could not answer what it charged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// The estimate was inside the budget.
    WithinBudget,
    /// Nothing here could be compared to a budget: a planner's cost units,
    /// which are not money, or a driver that does not estimate at all, or no
    /// budget configured.
    ///
    /// Approved rather than refused. The mock does not estimate and CI has
    /// nothing else, so refusing would mean nothing runs under CI — the budget
    /// is a defence only where a driver volunteers a number in money, and the
    /// type gate is the one that always holds.
    Ungated,
    /// Somebody was shown the estimate and said yes.
    ByHand,
}

/// A query that costs more than the budget allows.
///
/// Not an error: over the threshold is a normal branch, and the answer to it
/// is a person. The statement comes back out so that approving runs *that*
/// one — rebuilding it from the buffer would run whatever the buffer says now,
/// which after an `$EDITOR` round trip need not be what was estimated.
#[derive(Debug, Clone, PartialEq)]
pub struct OverBudget {
    pub sql: ValidatedSql,
    pub max_rows: Option<u32>,
    pub estimate: Estimate,
    pub budget: u64,
}

impl ApprovedQuery {
    /// Private, and the only way any of the constructors below build one.
    const fn new(
        sql: ValidatedSql,
        max_rows: Option<u32>,
        estimate: Estimate,
        granted: Approval,
    ) -> Self {
        Self {
            sql,
            max_rows,
            estimate,
            granted,
        }
    }

    /// Approve when the estimate fits, and report it when it does not.
    ///
    /// `budget` is in bytes and is compared only against [`Estimate::Bytes`].
    /// A cost-unit estimate is not money and a fixed cutoff on one would be a
    /// superstition compiled into the client, so it passes — shown, never
    /// gating.
    ///
    /// # Errors
    ///
    /// [`OverBudget`], carrying everything needed to ask a person and then
    /// call [`Self::by_hand`] with the same statement.
    pub fn within(
        sql: ValidatedSql,
        max_rows: Option<u32>,
        estimate: Estimate,
        budget: Option<u64>,
    ) -> Result<Self, OverBudget> {
        match (estimate.bytes(), budget) {
            (Some(bytes), Some(budget)) if bytes > budget => Err(OverBudget {
                sql,
                max_rows,
                estimate,
                budget,
            }),
            (Some(_), Some(_)) => Ok(Self::new(sql, max_rows, estimate, Approval::WithinBudget)),
            // A number nobody compared to anything, which is what a cost
            // estimate, an absent one and an absent budget all amount to.
            _ => Ok(Self::new(sql, max_rows, estimate, Approval::Ungated)),
        }
    }

    /// Approve because somebody was shown the estimate and said yes.
    ///
    /// Takes an [`OverBudget`] rather than the parts, so the only way to reach
    /// it is to have been refused first — an answer to a question that was
    /// actually asked.
    #[must_use]
    pub fn by_hand(refused: OverBudget) -> Self {
        Self::new(
            refused.sql,
            refused.max_rows,
            refused.estimate,
            Approval::ByHand,
        )
    }

    #[must_use]
    pub fn text(&self) -> &str {
        self.sql.text()
    }

    #[must_use]
    pub const fn max_rows(&self) -> Option<u32> {
        self.max_rows
    }

    #[must_use]
    pub const fn estimate(&self) -> Estimate {
        self.estimate
    }

    #[must_use]
    pub const fn granted(&self) -> Approval {
        self.granted
    }
}

/// Each statement as a byte range: from just after the previous boundary to
/// the offset of its own `;`, or to the end of the text for a final statement
/// with no semicolon.
///
/// A range rather than an end offset because a `;` that closed nothing is a
/// boundary all the same: `; select 1` is one statement, and starting it at
/// zero would hand the server a leading semicolon it will refuse.
///
/// A scanner rather than a parser. Nothing here needs a syntax tree: the
/// question is only where a statement ends, and the things that can hide a `;`
/// are few, listed, and testable — string and identifier quoting, the two
/// comment forms, and PostgreSQL's dollar quoting. A parser would answer the
/// same question with a dialect list to keep correct, and would put an AST in
/// reach of code that has no business branching on what kind of statement this
/// is. That is the server's judgement, not ours.
fn statement_spans(text: &str, escaping: Escaping) -> Result<Vec<(usize, usize)>, InvalidSql> {
    let bytes = text.as_bytes();
    let mut ends = Vec::new();
    let mut start = 0;
    let mut i = 0;
    // Whether anything but whitespace and comments has been seen since the
    // last boundary. A file of nothing but comments is not one statement, and
    // trimming cannot tell the two apart — `-- x` is not blank.
    let mut code = false;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                code = true;
                let quote = bytes[i];
                i += 1;
                loop {
                    // A backslash takes the next byte with it, so a quote
                    // behind one is not the end. Only where the server says so:
                    // reading `\\` as an escape against PostgreSQL would run
                    // the literal past its real end and merge two statements
                    // into one, which is the direction that runs something
                    // nobody asked for.
                    if escaping == Escaping::Backslash
                        && bytes.get(i) == Some(&b'\\')
                        && i + 1 < bytes.len()
                    {
                        i += 2;
                        continue;
                    }
                    let Some(&c) = bytes.get(i) else {
                        return Err(InvalidSql::Unterminated(match quote {
                            b'\'' => "string",
                            _ => "quoted name",
                        }));
                    };
                    if c != quote {
                        i += 1;
                        continue;
                    }
                    // A doubled quote is an escaped one and the literal goes
                    // on. Both dialects spell it this way.
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = memchr(bytes, b'\n', i).map_or(bytes.len(), |at| at + 1);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // Not nested: PostgreSQL nests block comments and BigQuery
                // does not, and the difference only matters for text that is
                // already a syntax error in one of them.
                let Some(at) = find(bytes, b"*/", i + 2) else {
                    return Err(InvalidSql::Unterminated("comment"));
                };
                i = at + 2;
            }
            b'$' => match dollar_tag(bytes, i) {
                Some(tag) => {
                    code = true;
                    let Some(at) = find(bytes, tag, i + tag.len()) else {
                        return Err(InvalidSql::Unterminated("dollar-quoted string"));
                    };
                    i = at + tag.len();
                }
                // `$1` and a bare `$` are not quoting.
                None => {
                    code = true;
                    i += 1;
                }
            },
            b';' => {
                if code {
                    // The offset *of* the semicolon, so the statement kept is
                    // the one the user wrote and an error position counted
                    // from its start still lands.
                    ends.push((start, i));
                }
                code = false;
                start = i + 1;
                i += 1;
            }
            c => {
                code |= !c.is_ascii_whitespace();
                i += 1;
            }
        }
    }

    // A last statement with no semicolon after it.
    if code {
        ends.push((start, bytes.len()));
    }
    Ok(ends)
}

/// The `$tag$` opening at `at`, if that is what it is.
///
/// A tag is `$`, an optional identifier, `$`. `$1` is a parameter and `$ ` is
/// not an opening at all, which is the whole of what has to be told apart.
fn dollar_tag(bytes: &[u8], at: usize) -> Option<&[u8]> {
    let mut end = at + 1;
    while let Some(&c) = bytes.get(end) {
        match c {
            b'$' => return Some(&bytes[at..=end]),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => end += 1,
            // A digit is allowed in a tag but not as its first character:
            // `$1$` would otherwise read as an opening rather than as a
            // parameter followed by a dollar.
            b'0'..=b'9' if end > at + 1 => end += 1,
            _ => return None,
        }
    }
    None
}

fn memchr(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|i| i + from)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PostgreSQL reading: a backslash is an ordinary character.
    fn one(text: &str) -> Result<ValidatedSql, InvalidSql> {
        ValidatedSql::parse(&RawSql::new(text), Escaping::None)
    }

    /// The BigQuery reading: a backslash escapes what follows it.
    fn one_escaped(text: &str) -> Result<ValidatedSql, InvalidSql> {
        ValidatedSql::parse(&RawSql::new(text), Escaping::Backslash)
    }

    #[test]
    fn one_statement_keeps_its_text_without_the_semicolon() {
        assert_eq!(one("select 1").unwrap().text(), "select 1");
        assert_eq!(one("  select 1;  ").unwrap().text(), "select 1");
        assert_eq!(one("select 1;\n").unwrap().text(), "select 1");
    }

    #[test]
    fn nothing_to_run_is_not_a_statement() {
        for empty in [
            "",
            "   ",
            "\n\n",
            ";",
            ";;",
            "-- just a comment",
            "/* and */",
        ] {
            assert_eq!(one(empty), Err(InvalidSql::Empty), "{empty:?}");
        }
    }

    #[test]
    fn a_semicolon_that_closed_nothing_is_not_part_of_the_statement() {
        // A stray leading `;` — left over from the statement above it, or from
        // a selection that started one character early. Keeping it would send
        // the server text it refuses for a reason that is not the user's.
        assert_eq!(one("; select 1").unwrap().text(), "select 1");
        assert_eq!(one(";;\nselect 1;").unwrap().text(), "select 1");
        assert_eq!(one("-- a\n;\nselect 1").unwrap().text(), "select 1");
    }

    #[test]
    fn several_statements_are_refused_and_counted() {
        // The case this exists for: the simple query protocol would run both
        // and report success, and a grid showing only the last result hides
        // the first.
        assert_eq!(
            one("drop table x; select 1"),
            Err(InvalidSql::Several { count: 2 })
        );
        assert_eq!(
            one("select 1; select 2; select 3;"),
            Err(InvalidSql::Several { count: 3 })
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_is_not_a_statement_boundary() {
        assert_eq!(one("select ';'").unwrap().text(), "select ';'");
        assert_eq!(
            one("select 'a;b', \"c;d\"").unwrap().text(),
            "select 'a;b', \"c;d\""
        );
        // Backticks, because BigQuery quotes names with them.
        assert_eq!(
            one("select * from `p.d.t;x`").unwrap().text(),
            "select * from `p.d.t;x`"
        );
    }

    #[test]
    fn a_doubled_quote_does_not_end_the_literal() {
        // `'it''s; fine'` is one string containing a semicolon. Reading the
        // second quote as the end would leave `s; fine'` outside it and split
        // the statement in two.
        assert_eq!(
            one("select 'it''s; fine'").unwrap().text(),
            "select 'it''s; fine'"
        );
        assert!(
            one("select 'a''; select 2").is_err(),
            "the string never ends"
        );
    }

    #[test]
    fn a_backslash_ends_a_literal_or_not_depending_on_the_server() {
        // One BigQuery statement: the escaped quote is inside the string, and
        // so is the semicolon after it.
        let bq = "select '\\'; select 1'";
        assert_eq!(one_escaped(bq).unwrap().text(), bq);

        // The same text against PostgreSQL, where `standard_conforming_strings`
        // makes the backslash ordinary: the literal ends at the second quote,
        // the `;` after it is a real boundary, and what follows opens a quote
        // that never closes. Which is also what the server sees — so the two
        // readings disagree about this text because the two servers do.
        assert_eq!(one(bq), Err(InvalidSql::Unterminated("string")));
    }

    #[test]
    fn a_literal_ending_in_an_escaped_quote_is_not_unterminated() {
        assert_eq!(one_escaped("select '\\''").unwrap().text(), "select '\\''");
        // A trailing backslash with nothing after it escapes nothing, and the
        // string is genuinely unterminated rather than swallowing the end.
        assert_eq!(
            one_escaped("select 'a\\"),
            Err(InvalidSql::Unterminated("string"))
        );
    }

    #[test]
    fn a_doubled_quote_still_works_where_backslashes_are_escapes() {
        // BigQuery has both spellings, and adding one must not remove the
        // other.
        assert_eq!(
            one_escaped("select 'it''s; fine'").unwrap().text(),
            "select 'it''s; fine'"
        );
    }

    #[test]
    fn a_semicolon_inside_a_comment_is_not_a_boundary() {
        assert_eq!(
            one("select 1 -- ; not this\n").unwrap().text(),
            "select 1 -- ; not this"
        );
        assert_eq!(one("select /* ; */ 1").unwrap().text(), "select /* ; */ 1");
        // A line comment with no newline after it still ends the text.
        assert_eq!(one("select 1 --; end").unwrap().text(), "select 1 --; end");
    }

    #[test]
    fn a_semicolon_inside_a_dollar_quoted_body_is_not_a_boundary() {
        // A function body is the case: it is full of semicolons and is one
        // statement.
        let body =
            "create function f() returns int as $$ begin; return 1; end; $$ language plpgsql";
        assert_eq!(one(body).unwrap().text(), body);
        assert_eq!(
            one("select $tag$ a; b $tag$").unwrap().text(),
            "select $tag$ a; b $tag$"
        );
    }

    #[test]
    fn a_parameter_is_not_a_dollar_quote() {
        // `$1` opening a dollar-quoted string would swallow the rest of the
        // text and turn two statements into one.
        assert_eq!(
            one("select $1; select $2"),
            Err(InvalidSql::Several { count: 2 })
        );
        assert_eq!(one("select 1 $ 2").unwrap().text(), "select 1 $ 2");
    }

    #[test]
    fn text_that_runs_off_the_end_says_what_never_closed() {
        assert_eq!(one("select 'a"), Err(InvalidSql::Unterminated("string")));
        assert_eq!(
            one("select \"a"),
            Err(InvalidSql::Unterminated("quoted name"))
        );
        assert_eq!(one("select /* a"), Err(InvalidSql::Unterminated("comment")));
        assert_eq!(
            one("select $$ a"),
            Err(InvalidSql::Unterminated("dollar-quoted string"))
        );
    }

    #[test]
    fn an_offset_becomes_a_line_and_a_column() {
        let text = "select\n  bad\nfrom t";
        // The `b` of `bad`: 10th character, counting the newlines.
        assert_eq!(Position::of_offset(text, 10), Some(Position::new(2, 3)));
        assert_eq!(Position::of_offset(text, 1), Some(Position::new(1, 1)));
        assert_eq!(
            Position::of_offset(text, u32::try_from(text.chars().count()).unwrap()),
            Some(Position::new(3, 6))
        );
    }

    #[test]
    fn a_position_that_is_not_one_is_no_position() {
        // Zero is what a server says for "no position", and past the end
        // would put a marker on a line nobody can see.
        assert_eq!(Position::of_offset("select 1", 0), None);
        assert_eq!(Position::of_offset("select 1", 9), None);
        assert_eq!(Position::of_offset("", 1), None);
    }

    #[test]
    fn a_column_is_counted_in_characters_rather_than_bytes() {
        // The server counts characters, so a name with an accent in it must
        // not shift the marker.
        assert_eq!(
            Position::of_offset("select é, bad", 11),
            Some(Position::new(1, 11))
        );
    }

    #[test]
    fn a_position_in_the_statement_is_a_position_in_what_was_typed() {
        // The blank lines above the statement never reached the server, so
        // its line two is line four of the buffer somebody is looking at.
        let sql = one("\n\nselect\n  bad\nfrom t").unwrap();
        assert_eq!(sql.in_source(Position::new(2, 3)), Position::new(4, 3));

        // On the statement's own first line the column moves as well.
        let sql = one("   select bad").unwrap();
        assert_eq!(sql.in_source(Position::new(1, 8)), Position::new(1, 11));

        // And nothing dropped means nothing moved.
        let sql = one("select bad").unwrap();
        assert_eq!(sql.in_source(Position::new(1, 8)), Position::new(1, 8));
    }

    #[test]
    fn a_budget_only_bites_on_a_number_that_is_money() {
        let sql = one("select 1").unwrap();
        let over = ApprovedQuery::within(sql.clone(), None, Estimate::Bytes(2_000), Some(1_000));
        assert!(over.is_err());

        // Planner cost units are not money, and a fixed cutoff on them would
        // be a superstition compiled into the client.
        let cost = ApprovedQuery::within(sql.clone(), None, Estimate::Cost(1e9), Some(1_000));
        assert_eq!(cost.unwrap().granted(), Approval::Ungated);

        let under = ApprovedQuery::within(sql, None, Estimate::Bytes(10), Some(1_000));
        assert_eq!(under.unwrap().granted(), Approval::WithinBudget);
    }

    #[test]
    fn a_driver_that_cannot_estimate_is_approved_rather_than_refused() {
        // The mock does not estimate, and CI has nothing else: refusing here
        // would mean nothing runs under CI at all. The budget defends only
        // where a driver volunteers a number; the type gate always holds.
        let sql = one("select 1").unwrap();
        let q = ApprovedQuery::within(sql, None, Estimate::Unknown, Some(0)).unwrap();
        assert_eq!(q.granted(), Approval::Ungated);
    }

    #[test]
    fn no_budget_approves_whatever_it_costs() {
        let sql = one("select 1").unwrap();
        let q = ApprovedQuery::within(sql, None, Estimate::Bytes(u64::MAX), None).unwrap();
        assert_eq!(q.granted(), Approval::Ungated);
    }

    #[test]
    fn approving_by_hand_runs_the_statement_that_was_estimated() {
        // Not one rebuilt from the buffer, which after an `$EDITOR` round trip
        // need not be what the number was for.
        let sql = one("select huge from t").unwrap();
        let refused =
            ApprovedQuery::within(sql, Some(50), Estimate::Bytes(9), Some(1)).unwrap_err();
        assert_eq!(refused.budget, 1);

        let q = ApprovedQuery::by_hand(refused);
        assert_eq!(q.text(), "select huge from t");
        assert_eq!(q.max_rows(), Some(50));
        assert_eq!(q.estimate(), Estimate::Bytes(9));
        assert_eq!(q.granted(), Approval::ByHand);
    }

    #[test]
    fn the_row_cap_never_reaches_the_statement() {
        // Appending a `LIMIT` would change the offsets errors are reported at,
        // and is wrong outright for a CTE ending in `INSERT … RETURNING`.
        let sql = one("with w as (insert into t values (1) returning *) select * from w").unwrap();
        let text = sql.text().to_owned();
        let q = ApprovedQuery::within(sql, Some(10), Estimate::Unknown, None).unwrap();
        assert_eq!(q.text(), text);
    }
}
