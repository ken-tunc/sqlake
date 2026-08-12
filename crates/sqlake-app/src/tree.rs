//! Lazy expansion state for the object tree, and the flattening that the UI
//! consumes.
//!
//! Expansion drives loading, so it is data and lives here rather than in the
//! view. What the UI receives is already flat: drawing the tree is a slice and
//! an index, never a recursive walk (decision D7).
//!
//! Scroll position and selection are deliberately *not* here. They belong to
//! `UiState`; mixing them in makes the tree jump on every asynchronous update.

use std::collections::HashMap;

use sqlake_core::node::{NodeRef, RelationKind, TreeNode};

/// How a node appears in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    /// Has no children at all, so no toggle is drawn.
    Leaf,
    Collapsed,
    Loading,
    Expanded,
    Failed(String),
}

impl NodeState {
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        matches!(self, Self::Expanded)
    }

    /// Whether a toggle glyph should be drawn at all.
    #[must_use]
    pub const fn is_toggleable(&self) -> bool {
        !matches!(self, Self::Leaf)
    }
}

/// One row of the flattened tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleNode {
    pub depth: u16,
    pub label: String,
    pub node_ref: NodeRef,
    pub relation_kind: Option<RelationKind>,
    pub state: NodeState,
}

/// The flattened tree handed to the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeView {
    pub nodes: Vec<VisibleNode>,
}

impl TreeView {
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<&VisibleNode> {
        self.nodes.get(index)
    }
}

/// What the caller must do after a toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Children must be fetched. The node is already showing as loading.
    Load,
    /// Handled entirely in memory.
    Local,
}

#[derive(Debug, Default)]
pub struct TreeState {
    /// Children that have been fetched, keyed by their parent.
    loaded: HashMap<NodeRef, Vec<TreeNode>>,
    /// Expansion status. Leaves never appear here.
    status: HashMap<NodeRef, NodeState>,
}

impl TreeState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the top level, as returned when a connection opens.
    pub fn set_roots(&mut self, roots: Vec<TreeNode>) {
        self.loaded.insert(NodeRef::root(), roots);
    }

    #[must_use]
    pub fn is_loaded(&self, node: &NodeRef) -> bool {
        self.loaded.contains_key(node)
    }

    /// Expand or collapse. Returns whether the caller has to fetch anything.
    ///
    /// Re-toggling a failed node retries it, which is the only sensible reading
    /// of clicking on an error.
    pub fn toggle(&mut self, node: &NodeRef) -> Toggle {
        match self.status.get(node) {
            // Already in flight. Clicking again should not start a second fetch.
            Some(NodeState::Loading) => Toggle::Local,
            Some(NodeState::Expanded) => {
                self.status.insert(node.clone(), NodeState::Collapsed);
                Toggle::Local
            }
            None | Some(NodeState::Collapsed | NodeState::Failed(_)) => {
                if self.is_loaded(node) && !self.has_failed(node) {
                    self.status.insert(node.clone(), NodeState::Expanded);
                    Toggle::Local
                } else {
                    self.status.insert(node.clone(), NodeState::Loading);
                    Toggle::Load
                }
            }
            // Leaves are never recorded, so this is unreachable in practice.
            Some(NodeState::Leaf) => Toggle::Local,
        }
    }

    fn has_failed(&self, node: &NodeRef) -> bool {
        matches!(self.status.get(node), Some(NodeState::Failed(_)))
    }

    /// Record the result of a fetch started by [`TreeState::toggle`].
    pub fn finish_load(&mut self, node: &NodeRef, result: Result<Vec<TreeNode>, String>) {
        match result {
            Ok(children) => {
                self.loaded.insert(node.clone(), children);
                self.status.insert(node.clone(), NodeState::Expanded);
            }
            Err(message) => {
                // Drop any stale children so a retry refetches rather than
                // showing what was there before the failure.
                self.loaded.remove(node);
                self.status.insert(node.clone(), NodeState::Failed(message));
            }
        }
    }

    /// Depth-first flattening of everything currently visible.
    #[must_use]
    pub fn flatten(&self) -> TreeView {
        let mut nodes = Vec::new();
        self.walk(&NodeRef::root(), 0, &mut nodes);
        TreeView { nodes }
    }

    fn walk(&self, parent: &NodeRef, depth: u16, out: &mut Vec<VisibleNode>) {
        let Some(children) = self.loaded.get(parent) else {
            return;
        };
        for child in children {
            let state = self.state_of(child);
            let expanded = state.is_expanded();
            out.push(VisibleNode {
                depth,
                label: child.label.clone(),
                node_ref: child.node_ref.clone(),
                relation_kind: child.relation_kind,
                state,
            });
            if expanded {
                self.walk(&child.node_ref, depth + 1, out);
            }
        }
    }

    fn state_of(&self, node: &TreeNode) -> NodeState {
        if !node.has_children {
            return NodeState::Leaf;
        }
        self.status
            .get(&node.node_ref)
            .cloned()
            .unwrap_or(NodeState::Collapsed)
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::node::NodeKind;

    use super::*;

    fn schema(name: &str) -> TreeNode {
        TreeNode::branch(NodeRef::new(NodeKind::Namespace, [name]), name)
    }

    fn table(schema: &str, name: &str) -> TreeNode {
        TreeNode::relation(
            NodeRef::new(NodeKind::Relation, [schema, name]),
            name,
            RelationKind::Table,
        )
    }

    fn state_with_roots() -> TreeState {
        let mut t = TreeState::new();
        t.set_roots(vec![schema("public"), schema("analytics")]);
        t
    }

    fn labels(view: &TreeView) -> Vec<(u16, &str)> {
        view.nodes
            .iter()
            .map(|n| (n.depth, n.label.as_str()))
            .collect()
    }

    #[test]
    fn an_unexpanded_tree_shows_only_the_top_level() {
        let t = state_with_roots();
        assert_eq!(labels(&t.flatten()), [(0, "public"), (0, "analytics")]);
        assert_eq!(t.flatten().nodes[0].state, NodeState::Collapsed);
    }

    #[test]
    fn the_first_toggle_asks_for_a_load_and_shows_it() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        assert_eq!(t.toggle(&node), Toggle::Load);
        assert_eq!(t.flatten().nodes[0].state, NodeState::Loading);
    }

    #[test]
    fn a_second_toggle_while_loading_does_not_start_another_fetch() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        assert_eq!(t.toggle(&node), Toggle::Load);
        assert_eq!(t.toggle(&node), Toggle::Local);
        assert_eq!(t.flatten().nodes[0].state, NodeState::Loading);
    }

    #[test]
    fn loaded_children_appear_indented_under_their_parent() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        t.toggle(&node);
        t.finish_load(&node, Ok(vec![table("public", "users")]));

        assert_eq!(
            labels(&t.flatten()),
            [(0, "public"), (1, "users"), (0, "analytics")]
        );
    }

    #[test]
    fn relations_are_leaves_and_draw_no_toggle() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        t.toggle(&node);
        t.finish_load(&node, Ok(vec![table("public", "users")]));

        let view = t.flatten();
        assert_eq!(view.nodes[1].state, NodeState::Leaf);
        assert!(!view.nodes[1].state.is_toggleable());
    }

    #[test]
    fn collapsing_hides_children_without_discarding_them() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        t.toggle(&node);
        t.finish_load(&node, Ok(vec![table("public", "users")]));

        assert_eq!(t.toggle(&node), Toggle::Local);
        assert_eq!(labels(&t.flatten()), [(0, "public"), (0, "analytics")]);

        // Re-expanding is local: the children are still in memory.
        assert_eq!(t.toggle(&node), Toggle::Local);
        assert_eq!(t.flatten().len(), 3);
    }

    #[test]
    fn a_failure_is_shown_on_the_node_that_failed() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["analytics"]);
        t.toggle(&node);
        t.finish_load(&node, Err("permission denied".into()));

        let view = t.flatten();
        assert_eq!(
            view.nodes[1].state,
            NodeState::Failed("permission denied".into())
        );
        // The failure is local to the node; its sibling is untouched.
        assert_eq!(view.nodes[0].state, NodeState::Collapsed);
    }

    #[test]
    fn toggling_a_failed_node_retries_the_fetch() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["analytics"]);
        t.toggle(&node);
        t.finish_load(&node, Err("temporary".into()));

        assert_eq!(t.toggle(&node), Toggle::Load);
        t.finish_load(&node, Ok(vec![table("analytics", "events")]));
        assert_eq!(
            labels(&t.flatten()),
            [(0, "public"), (0, "analytics"), (1, "events")]
        );
    }

    #[test]
    fn a_failed_load_drops_stale_children() {
        let mut t = state_with_roots();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        t.toggle(&node);
        t.finish_load(&node, Ok(vec![table("public", "users")]));
        t.toggle(&node); // collapse
        t.toggle(&node); // expand again, locally
        t.finish_load(&node, Err("gone".into()));

        assert!(!t.is_loaded(&node));
        assert_eq!(labels(&t.flatten()), [(0, "public"), (0, "analytics")]);
    }

    #[test]
    fn nesting_goes_deeper_than_two_levels() {
        let mut t = TreeState::new();
        t.set_roots(vec![TreeNode::branch(
            NodeRef::new(NodeKind::Catalog, ["db"]),
            "db",
        )]);
        let db = NodeRef::new(NodeKind::Catalog, ["db"]);
        t.toggle(&db);
        t.finish_load(
            &db,
            Ok(vec![TreeNode::branch(
                NodeRef::new(NodeKind::Namespace, ["db", "public"]),
                "public",
            )]),
        );
        let schema = NodeRef::new(NodeKind::Namespace, ["db", "public"]);
        t.toggle(&schema);
        t.finish_load(
            &schema,
            Ok(vec![TreeNode::relation(
                NodeRef::new(NodeKind::Relation, ["db", "public", "users"]),
                "users",
                RelationKind::Table,
            )]),
        );

        assert_eq!(
            labels(&t.flatten()),
            [(0, "db"), (1, "public"), (2, "users")]
        );
    }

    #[test]
    fn an_empty_tree_flattens_to_nothing() {
        assert!(TreeState::new().flatten().is_empty());
    }
}
