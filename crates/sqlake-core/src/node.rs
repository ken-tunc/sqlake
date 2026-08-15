//! Positions in the object hierarchy.

use std::fmt;

/// A structural level in the object hierarchy.
///
/// Deliberately generic: PostgreSQL's schema and BigQuery's dataset are both
/// [`NodeKind::Namespace`]. The word shown to the user comes from
/// [`crate::capability::HierarchyLevel::label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeKind {
    /// The connection itself.
    Root,
    /// A database or project.
    Catalog,
    /// A schema or dataset.
    Namespace,
    /// A table, view or similar.
    Relation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    Routine,
    External,
}

impl RelationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "matview",
            Self::Routine => "routine",
            Self::External => "external",
        }
    }
}

/// A position in the hierarchy.
///
/// The number of levels is driver-dependent, so this is a path rather than a
/// fixed set of fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeRef {
    pub kind: NodeKind,
    pub path: Vec<String>,
}

impl NodeRef {
    #[must_use]
    pub const fn root() -> Self {
        Self {
            kind: NodeKind::Root,
            path: Vec::new(),
        }
    }

    pub fn new(kind: NodeKind, path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            kind,
            path: path.into_iter().map(Into::into).collect(),
        }
    }

    /// The last path segment, which is the node's own name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.path.last().map(String::as_str)
    }

    /// How deep this node sits. The root is 0.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    pub fn child(&self, kind: NodeKind, name: impl Into<String>) -> Self {
        let mut path = self.path.clone();
        path.push(name.into());
        Self { kind, path }
    }

    /// A [`TableRef`] if this node is a relation.
    #[must_use]
    pub fn as_table(&self) -> Option<TableRef> {
        (self.kind == NodeKind::Relation).then(|| TableRef {
            path: self.path.clone(),
        })
    }
}

impl fmt::Display for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str("<root>")
        } else {
            f.write_str(&self.path.join("."))
        }
    }
}

/// A fully qualified relation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub path: Vec<String>,
}

impl TableRef {
    pub fn new(path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    #[must_use]
    pub fn to_node_ref(&self) -> NodeRef {
        NodeRef {
            kind: NodeKind::Relation,
            path: self.path.clone(),
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path.join("."))
    }
}

/// One entry returned by a driver when a node is expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub node_ref: NodeRef,
    /// What the user reads. Usually the last path segment, but not always —
    /// the root shows the connection name.
    pub label: String,
    /// Whether to draw an expand toggle. Drivers that cannot answer cheaply
    /// should say `true` and return an empty child list.
    pub has_children: bool,
    pub relation_kind: Option<RelationKind>,
}

impl TreeNode {
    pub fn branch(node_ref: NodeRef, label: impl Into<String>) -> Self {
        Self {
            node_ref,
            label: label.into(),
            has_children: true,
            relation_kind: None,
        }
    }

    pub fn relation(node_ref: NodeRef, label: impl Into<String>, kind: RelationKind) -> Self {
        Self {
            node_ref,
            label: label.into(),
            has_children: false,
            relation_kind: Some(kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_extends_the_path() {
        let schema = NodeRef::new(NodeKind::Namespace, ["public"]);
        let table = schema.child(NodeKind::Relation, "users");
        assert_eq!(table.path, ["public", "users"]);
        assert_eq!(table.name(), Some("users"));
        assert_eq!(table.depth(), 2);
    }

    #[test]
    fn only_relations_convert_to_table_refs() {
        let schema = NodeRef::new(NodeKind::Namespace, ["public"]);
        assert!(schema.as_table().is_none());

        let table = NodeRef::new(NodeKind::Relation, ["public", "users"]);
        assert_eq!(
            table.as_table().map(|t| t.to_string()).as_deref(),
            Some("public.users")
        );
    }

    #[test]
    fn a_table_ref_round_trips_through_a_node_ref() {
        let table = TableRef::new(["public", "users"]);
        assert_eq!(table.to_node_ref().as_table(), Some(table));
    }

    #[test]
    fn the_root_displays_as_root() {
        assert_eq!(NodeRef::root().to_string(), "<root>");
        assert_eq!(NodeRef::root().name(), None);
    }

    #[test]
    fn dots_in_a_name_do_not_change_the_depth() {
        // A relation literally named "a.b" is still one path segment.
        let node = NodeRef::new(NodeKind::Relation, ["public", "a.b"]);
        assert_eq!(node.depth(), 2);
        assert_eq!(node.name(), Some("a.b"));
    }
}
