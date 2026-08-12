//! An in-memory driver used to develop and test everything else without a
//! database.
//!
//! It injects latency and failures on purpose. Without them, loading states and
//! error surfaces get written blind, and a UI that blocks is only discovered
//! once a real driver lands on top of it.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlake_core::capability::{Capabilities, DriverKind, HierarchyLevel, QuoteStyle};
use sqlake_core::driver::{Driver, DriverError, DriverResult, Session};
use sqlake_core::node::{NodeKind, NodeRef, TableRef, TreeNode};
use sqlake_core::result::{PageRequest, ResultSet, Row, Sort, SortDir};
use sqlake_core::value::Value;

pub mod fixtures;

use fixtures::Catalog;

/// A two-level hierarchy, deliberately shorter than PostgreSQL's three. If the
/// tree only ever sees one shape, the shape is not really data-driven.
const HIERARCHY: &[HierarchyLevel] = &[
    HierarchyLevel::new(NodeKind::Namespace, "schema"),
    HierarchyLevel::new(NodeKind::Relation, "table"),
];

const CAPABILITIES: Capabilities = Capabilities {
    hierarchy: HIERARCHY,
    indexes: false,
    triggers: false,
    constraints: false,
    partitioning: false,
    transactions: false,
    cancel: true,
    streaming: false,
    cost_estimate: false,
    free_preview: true,
    quote_style: QuoteStyle::DoubleQuote,
};

/// How the mock should misbehave.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// Applied to every call.
    pub latency: Duration,
    /// Node paths whose expansion or preview always fails.
    pub failing_nodes: Vec<Vec<String>>,
    /// Node paths that take [`Behaviour::slow_latency`] instead.
    pub slow_nodes: Vec<Vec<String>>,
    pub slow_latency: Duration,
}

impl Behaviour {
    /// No delays, no failures. For tests that are not about either.
    #[must_use]
    pub fn instant() -> Self {
        Self::default()
    }

    /// The default for interactive use: long enough that a missing spinner is
    /// obvious, short enough to stay usable.
    #[must_use]
    pub fn fixture() -> Self {
        Self {
            latency: Duration::from_millis(120),
            failing_nodes: vec![
                vec!["restricted".to_owned()],
                vec!["analytics".to_owned(), "broken".to_owned()],
            ],
            slow_nodes: vec![vec!["analytics".to_owned(), "slow".to_owned()]],
            slow_latency: Duration::from_secs(2),
        }
    }

    fn matches(list: &[Vec<String>], path: &[String]) -> bool {
        list.iter().any(|p| p == path)
    }

    async fn delay_for(&self, path: &[String]) {
        let d = if Self::matches(&self.slow_nodes, path) {
            self.slow_latency
        } else {
            self.latency
        };
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
    }

    fn fails_for(&self, path: &[String]) -> bool {
        Self::matches(&self.failing_nodes, path)
    }
}

#[derive(Debug)]
pub struct MockDriver {
    behaviour: Behaviour,
    catalog: Arc<Catalog>,
}

impl MockDriver {
    #[must_use]
    pub fn new(behaviour: Behaviour) -> Self {
        Self {
            behaviour,
            catalog: Arc::new(fixtures::catalog()),
        }
    }
}

impl Default for MockDriver {
    fn default() -> Self {
        Self::new(Behaviour::fixture())
    }
}

#[async_trait]
impl Driver for MockDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Mock
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self) -> DriverResult<Box<dyn Session>> {
        self.behaviour.delay_for(&[]).await;
        Ok(Box::new(MockSession {
            behaviour: self.behaviour.clone(),
            catalog: Arc::clone(&self.catalog),
        }))
    }
}

#[derive(Debug)]
pub struct MockSession {
    behaviour: Behaviour,
    catalog: Arc<Catalog>,
}

#[async_trait]
impl Session for MockSession {
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn children(&self, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
        self.behaviour.delay_for(&of.path).await;
        if self.behaviour.fails_for(&of.path) {
            return Err(DriverError::Query(format!("permission denied for {of}")));
        }

        match of.kind {
            NodeKind::Root => Ok(self
                .catalog
                .schemas
                .iter()
                .map(|s| TreeNode::branch(of.child(NodeKind::Namespace, s.name), s.name))
                .collect()),

            NodeKind::Namespace => {
                let name = of.name().unwrap_or_default();
                let schema = self
                    .catalog
                    .schema(name)
                    .ok_or_else(|| DriverError::NotFound(format!("schema {name}")))?;
                Ok(schema
                    .tables
                    .iter()
                    .map(|t| {
                        TreeNode::relation(of.child(NodeKind::Relation, t.name), t.name, t.kind)
                    })
                    .collect())
            }

            // Relations are leaves until M5 adds columns to the tree.
            NodeKind::Relation | NodeKind::Catalog => Ok(Vec::new()),
        }
    }

    async fn preview(&self, table: &TableRef, req: &PageRequest) -> DriverResult<ResultSet> {
        self.behaviour.delay_for(&table.path).await;
        if self.behaviour.fails_for(&table.path) {
            return Err(DriverError::Query(format!(
                "relation {table} is corrupt: unexpected page header"
            )));
        }

        let [schema, name] = table.path.as_slice() else {
            return Err(DriverError::NotFound(format!("{table} is not a relation")));
        };
        let fixture = self
            .catalog
            .table(schema, name)
            .ok_or_else(|| DriverError::NotFound(table.to_string()))?;

        let rows = match req.sort {
            // The lazy path, and the one that matters: only the requested page
            // is ever built.
            None => fixture.page(req.offset, req.limit),
            // Sorting needs the whole relation. A real driver pushes this down
            // to the engine; the mock is allowed to be direct about it.
            Some(sort) => {
                let mut all = fixture.all_rows();
                sort_rows(&mut all, sort);
                all.into_iter()
                    .skip(usize::try_from(req.offset).unwrap_or(usize::MAX))
                    .take(req.limit as usize)
                    .collect()
            }
        };

        Ok(ResultSet::new(
            fixture.columns.clone(),
            rows,
            Some(fixture.total_rows()),
        ))
    }

    async fn close(self: Box<Self>) {}
}

fn sort_rows(rows: &mut [Row], sort: Sort) {
    rows.sort_by(|a, b| {
        let ord = compare(a.get(sort.column), b.get(sort.column));
        match sort.dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    });
}

/// Ordering for sorting only. Nulls sort last in ascending order, which is what
/// PostgreSQL does by default.
fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    match (a, b) {
        (None | Some(Value::Null), None | Some(Value::Null)) => Ordering::Equal,
        (None | Some(Value::Null), _) => Ordering::Greater,
        (_, None | Some(Value::Null)) => Ordering::Less,
        (Some(a), Some(b)) => match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Date(x), Value::Date(y)) => x.cmp(y),
            (Value::Time(x), Value::Time(y)) => x.cmp(y),
            (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
            (Value::TimestampTz(x), Value::TimestampTz(y)) => x.cmp(y),
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            // Decimals are text, so compare them numerically where possible.
            (Value::Decimal(x), Value::Decimal(y)) => match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => x.cmp(y),
            },
            // Mixed or structured values have no meaningful order. Leaving them
            // equal keeps the sort stable rather than arbitrary.
            _ => Ordering::Equal,
        },
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::node::RelationKind;

    use super::*;

    async fn session() -> Box<dyn Session> {
        MockDriver::new(Behaviour::instant())
            .connect()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_root_lists_schemas() {
        let s = session().await;
        let nodes = s.children(&NodeRef::root()).await.unwrap();
        let names: Vec<_> = nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(names, ["public", "analytics", "restricted"]);
        assert!(nodes.iter().all(|n| n.has_children));
    }

    #[tokio::test]
    async fn a_schema_lists_its_relations() {
        let s = session().await;
        let schema = NodeRef::new(NodeKind::Namespace, ["public"]);
        let nodes = s.children(&schema).await.unwrap();
        assert!(nodes.iter().any(|n| n.label == "users"));
        assert!(nodes.iter().all(|n| !n.has_children));
        assert!(
            nodes
                .iter()
                .any(|n| n.relation_kind == Some(RelationKind::Table))
        );
    }

    #[tokio::test]
    async fn views_are_reported_as_views() {
        let s = session().await;
        let schema = NodeRef::new(NodeKind::Namespace, ["analytics"]);
        let nodes = s.children(&schema).await.unwrap();
        let view = nodes.iter().find(|n| n.label == "daily_summary").unwrap();
        assert_eq!(view.relation_kind, Some(RelationKind::View));
    }

    #[tokio::test]
    async fn a_failing_node_reports_an_error_rather_than_an_empty_list() {
        let driver = MockDriver::new(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        });
        let s = driver.connect().await.unwrap();
        let node = NodeRef::new(NodeKind::Namespace, ["restricted"]);
        let err = s.children(&node).await.unwrap_err();
        assert!(matches!(err, DriverError::Query(_)), "{err:?}");
    }

    #[tokio::test]
    async fn preview_returns_the_requested_page_only() {
        let s = session().await;
        let table = TableRef::new(["public", "big"]);
        let req = PageRequest {
            offset: 1000,
            limit: 10,
            sort: None,
        };
        let rs = s.preview(&table, &req).await.unwrap();
        assert_eq!(rs.row_count(), 10);
        assert_eq!(rs.total_rows, Some(fixtures::BIG_ROWS));
        assert_eq!(rs.rows[0].get(0), Some(&Value::Int(1000)));
    }

    #[tokio::test]
    async fn sorting_orders_the_whole_relation_not_just_the_page() {
        let s = session().await;
        let table = TableRef::new(["public", "users"]);
        let req = PageRequest {
            offset: 0,
            limit: 3,
            sort: Some(Sort::new(0, SortDir::Desc)),
        };
        let rs = s.preview(&table, &req).await.unwrap();
        assert_eq!(rs.rows[0].get(0), Some(&Value::Int(50)));
        assert_eq!(rs.rows[2].get(0), Some(&Value::Int(48)));
    }

    #[tokio::test]
    async fn nulls_sort_last_ascending() {
        let s = session().await;
        let table = TableRef::new(["public", "users"]);
        // `notes` is null on every fourth row.
        let req = PageRequest {
            offset: 0,
            limit: 50,
            sort: Some(Sort::new(7, SortDir::Asc)),
        };
        let rs = s.preview(&table, &req).await.unwrap();
        let first_null = rs.rows.iter().position(|r| r.get(7) == Some(&Value::Null));
        let last_value = rs.rows.iter().rposition(|r| r.get(7) != Some(&Value::Null));
        assert!(first_null.unwrap() > last_value.unwrap());
    }

    #[tokio::test]
    async fn previewing_a_broken_relation_fails() {
        let driver = MockDriver::new(Behaviour {
            failing_nodes: vec![vec!["analytics".to_owned(), "broken".to_owned()]],
            ..Behaviour::instant()
        });
        let s = driver.connect().await.unwrap();
        let err = s
            .preview(
                &TableRef::new(["analytics", "broken"]),
                &PageRequest::first(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_relation_is_not_found() {
        let s = session().await;
        let err = s
            .preview(&TableRef::new(["public", "nope"]), &PageRequest::first())
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_empty_relation_still_reports_its_columns() {
        let s = session().await;
        let rs = s
            .preview(&TableRef::new(["public", "empty"]), &PageRequest::first())
            .await
            .unwrap();
        assert_eq!(rs.row_count(), 0);
        assert_eq!(rs.column_count(), 2);
        assert_eq!(rs.total_rows, Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_nodes_take_the_slow_latency() {
        let driver = MockDriver::new(Behaviour {
            latency: Duration::from_millis(10),
            slow_nodes: vec![vec!["analytics".to_owned(), "slow".to_owned()]],
            slow_latency: Duration::from_secs(2),
            ..Behaviour::instant()
        });
        let s = driver.connect().await.unwrap();
        let start = tokio::time::Instant::now();
        s.preview(&TableRef::new(["analytics", "slow"]), &PageRequest::first())
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_secs(2));
    }
}
