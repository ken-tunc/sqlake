//! What a driver can and cannot do.
//!
//! The UI consults [`Capabilities`] to decide what to show. `if driver ==
//! Postgres` must never appear in the TUI crate: a difference between drivers
//! is a field here, or it is not expressed at all.

use crate::node::NodeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverKind {
    Postgres,
    BigQuery,
    Mock,
}

impl DriverKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::BigQuery => "bigquery",
            Self::Mock => "mock",
        }
    }
}

/// Whether a backslash escapes the next character inside a string literal.
///
/// The two dialects differ, and the difference is not cosmetic: it decides
/// where a string ends, and therefore where a *statement* ends. BigQuery
/// honours `\'`; PostgreSQL with `standard_conforming_strings` — on by default
/// since 9.1 — does not, and reads the backslash as an ordinary character.
///
/// Getting it wrong is wrong in both directions. Honouring a backslash where
/// the server does not merges two statements into one, which is the dangerous
/// way; ignoring one where the server honours it splits a valid statement in
/// two and refuses it, which is the merely annoying way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Escaping {
    /// `\'` is a quote inside the literal.
    Backslash,
    /// A backslash is an ordinary character, and `''` is the only escape.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuoteStyle {
    /// `"ident"` — PostgreSQL and the SQL standard.
    DoubleQuote,
    /// `` `ident` `` — BigQuery.
    Backtick,
}

/// One level of the object hierarchy.
///
/// `kind` is what the code branches on; `label` is what the user reads. Keeping
/// them apart is what lets BigQuery call a namespace a "dataset" and PostgreSQL
/// call it a "schema" without either name reaching a `match` in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HierarchyLevel {
    pub kind: NodeKind,
    pub label: &'static str,
}

impl HierarchyLevel {
    #[must_use]
    pub const fn new(kind: NodeKind, label: &'static str) -> Self {
        Self { kind, label }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The levels below the root, outermost first.
    pub hierarchy: &'static [HierarchyLevel],
    pub indexes: bool,
    pub triggers: bool,
    pub constraints: bool,
    pub partitioning: bool,
    pub transactions: bool,
    /// Cancelling reaches the server.
    ///
    /// Not "the client can stop waiting", which is always true and is done by
    /// dropping the request. This is the stronger claim that the work stops —
    /// which for a query that is being billed by the byte is the only version
    /// of the claim worth making.
    pub cancel: bool,
    /// When false, results are fetched in full before being displayed.
    pub streaming: bool,
    /// The driver can estimate the cost of a query before running it.
    pub cost_estimate: bool,
    /// Previewing a table is free, so it need not go through a query.
    pub free_preview: bool,
    /// A preview can be ordered by a column.
    ///
    /// Separate from [`Capabilities::free_preview`] because BigQuery answers
    /// the two differently: `tabledata.list` is not billed and cannot sort,
    /// and sorting means `SELECT … ORDER BY`, which is billed.
    pub sortable_preview: bool,
    pub quote_style: QuoteStyle,
    /// How a string literal escapes a quote, which is what decides where a
    /// statement ends. See [`Escaping`].
    pub escaping: Escaping,
}

impl Capabilities {
    /// Depth of the hierarchy below the root, i.e. the length of a full path.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.hierarchy.len()
    }

    /// The user-facing name for the level a node sits at.
    #[must_use]
    pub fn label_for(&self, kind: NodeKind) -> Option<&'static str> {
        self.hierarchy
            .iter()
            .find(|level| level.kind == kind)
            .map(|level| level.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Capabilities = Capabilities {
        hierarchy: &[
            HierarchyLevel::new(NodeKind::Catalog, "database"),
            HierarchyLevel::new(NodeKind::Namespace, "schema"),
            HierarchyLevel::new(NodeKind::Relation, "table"),
        ],
        indexes: true,
        triggers: true,
        constraints: true,
        partitioning: true,
        transactions: true,
        cancel: true,
        streaming: true,
        cost_estimate: true,
        free_preview: false,
        sortable_preview: true,
        quote_style: QuoteStyle::DoubleQuote,
        escaping: Escaping::None,
    };

    const BQ: Capabilities = Capabilities {
        hierarchy: &[
            HierarchyLevel::new(NodeKind::Catalog, "project"),
            HierarchyLevel::new(NodeKind::Namespace, "dataset"),
            HierarchyLevel::new(NodeKind::Relation, "table"),
        ],
        indexes: false,
        triggers: false,
        constraints: false,
        partitioning: true,
        transactions: false,
        cancel: false,
        streaming: true,
        cost_estimate: true,
        free_preview: true,
        sortable_preview: false,
        quote_style: QuoteStyle::Backtick,
        escaping: Escaping::Backslash,
    };

    #[test]
    fn the_same_structure_carries_different_words() {
        assert_eq!(PG.depth(), BQ.depth());
        assert_eq!(PG.label_for(NodeKind::Namespace), Some("schema"));
        assert_eq!(BQ.label_for(NodeKind::Namespace), Some("dataset"));
    }

    #[test]
    fn root_is_not_a_hierarchy_level() {
        assert_eq!(PG.label_for(NodeKind::Root), None);
    }
}
