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
    pub cancel: bool,
    /// When false, results are fetched in full before being displayed.
    pub streaming: bool,
    /// The driver can estimate the cost of a query before running it.
    pub cost_estimate: bool,
    /// Previewing a table is free, so it need not go through a query.
    pub free_preview: bool,
    pub quote_style: QuoteStyle,
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
        quote_style: QuoteStyle::DoubleQuote,
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
        cancel: true,
        streaming: true,
        cost_estimate: true,
        free_preview: true,
        quote_style: QuoteStyle::Backtick,
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
