//! What a relation *is*, as opposed to what is in it.
//!
//! Every driver answers the same shape and fills in only what it has, which is
//! why [`TableDetail::sections`] is a list rather than a field per kind of
//! thing: indexes, triggers, constraints, partitioning and clustering are all
//! a title and a table, and a front-end that can draw one can draw all of
//! them. BigQuery fills in partitioning and clustering and leaves the rest
//! empty without having to say so.

use crate::node::{RelationKind, TableRef};
use crate::result::ResultSet;

/// One column, as the catalogue describes it.
///
/// Its own type rather than [`Column`](crate::result::Column) with more fields
/// on it: a `Column` describes what came back from a query, and half of what
/// is here is not on the wire at all. Keeping them apart is what stops a page
/// being read as an answer about the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// The driver's own name for the type, shown as-is. Not interpreted.
    pub type_name: String,
    pub nullable: bool,
    /// The default expression, as the server stores it — `now()` rather than a
    /// value, because that is what it is.
    pub default: Option<String>,
    pub comment: Option<String>,
}

/// One table of facts about a relation, under a name the driver chose.
///
/// The title is the driver's because "Indexes" and "Clustering" are not the
/// same list under two names, and a fixed set of titles here would make
/// BigQuery answer an empty "Triggers" rather than not having one.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailSection {
    pub title: String,
    pub table: ResultSet,
}

/// The statement that would create this relation.
///
/// The two kinds are kept apart in the type rather than in a sentence next to
/// it, because the difference is the whole of what a reader needs and a string
/// loses it: one is what the server says, and the other is this client's
/// reading of a catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ddl {
    /// What the server itself reports — BigQuery's `INFORMATION_SCHEMA`.
    Reported(String),
    /// Built from the catalogue by this client, because the server offers
    /// nothing. Correct as far as it goes and never further: a front-end
    /// showing this has to say so, since somebody who copies it and runs it
    /// gets what it covers rather than what is there.
    Generated(String),
}

impl Ddl {
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Reported(text) | Self::Generated(text) => text,
        }
    }

    /// Whether this came from the server rather than from here.
    #[must_use]
    pub const fn is_reported(&self) -> bool {
        matches!(self, Self::Reported(_))
    }
}

/// Everything a driver can say about one relation.
#[derive(Debug, Clone, PartialEq)]
pub struct TableDetail {
    pub table: TableRef,
    pub kind: RelationKind,
    pub comment: Option<String>,
    pub columns: Vec<ColumnDef>,
    /// Whatever this driver has, in the order it wants them read. Empty is a
    /// normal answer.
    pub sections: Vec<DetailSection>,
    pub ddl: Option<Ddl>,
    /// Row count, size, last modified — named pairs rather than fields,
    /// because which of them exist differs per driver and a field nobody fills
    /// is a question every front-end has to answer with "unknown".
    pub stats: Vec<(String, String)>,
}

impl TableDetail {
    /// The least a driver can answer: what it is and what its columns are.
    #[must_use]
    pub fn new(table: TableRef, kind: RelationKind, columns: Vec<ColumnDef>) -> Self {
        Self {
            table,
            kind,
            comment: None,
            columns,
            sections: Vec::new(),
            ddl: None,
            stats: Vec::new(),
        }
    }

    /// The section under this title, if the driver filled one in.
    #[must_use]
    pub fn section(&self, title: &str) -> Option<&DetailSection> {
        self.sections.iter().find(|s| s.title == title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> TableDetail {
        TableDetail::new(
            TableRef::new(["public", "users"]),
            RelationKind::Table,
            vec![ColumnDef {
                name: "id".to_owned(),
                type_name: "int4".to_owned(),
                nullable: false,
                default: None,
                comment: None,
            }],
        )
    }

    #[test]
    fn a_driver_that_has_nothing_else_still_answers() {
        // Empty sections are the ordinary case, not a failure to fill them in:
        // BigQuery has no triggers to have.
        let detail = detail();
        assert!(detail.sections.is_empty());
        assert!(detail.ddl.is_none());
        assert_eq!(detail.columns.len(), 1);
    }

    #[test]
    fn a_section_is_found_by_the_title_its_driver_gave_it() {
        let mut detail = detail();
        detail.sections.push(DetailSection {
            title: "Indexes".to_owned(),
            table: ResultSet::empty(),
        });
        assert!(detail.section("Indexes").is_some());
        // Not by a title some other driver uses for its own list.
        assert!(detail.section("Clustering").is_none());
    }

    #[test]
    fn where_the_ddl_came_from_survives_being_read() {
        // A string would lose it, and losing it is how a reconstruction gets
        // copied and run as though it were the real thing.
        let reported = Ddl::Reported("CREATE TABLE t (…)".to_owned());
        let generated = Ddl::Generated("CREATE TABLE t (…)".to_owned());
        assert_eq!(reported.text(), generated.text());
        assert!(reported.is_reported());
        assert!(!generated.is_reported());
    }
}
