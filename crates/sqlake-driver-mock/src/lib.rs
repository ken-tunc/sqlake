//! An in-memory driver used to develop and test everything else without a
//! database.
//!
//! It injects latency and failures on purpose. Without them, loading states and
//! error surfaces get written blind, and a UI that blocks is only discovered
//! once a real driver lands on top of it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlake_core::capability::{Capabilities, DriverKind, HierarchyLevel, QuoteStyle};
use sqlake_core::driver::{Driver, DriverError, DriverResult, Session};
use sqlake_core::id::ProfileId;
use sqlake_core::node::{NodeKind, NodeRef, TableRef, TreeNode};
use sqlake_core::profile::{Params, ProfileError, ProfileSummary, Profiles, ResolvedProfile};
use sqlake_core::result::{PageRequest, ResultSet, Row, Sort, SortDir};
use sqlake_core::value::Value;

pub mod fixtures;

use fixtures::Catalog;

/// A profile the mock driver will accept.
///
/// Lives here rather than in each crate's tests because everything above this
/// one needs a connectable profile to test with, and a second hand-rolled copy
/// of it would drift from what [`MockDriver::connect`] actually checks.
///
/// # Panics
///
/// If `id` is not a usable [`ProfileId`]. Callers are tests and wiring code
/// with literal ids.
#[must_use]
pub fn mock_profile(id: &str) -> ResolvedProfile {
    ResolvedProfile {
        id: ProfileId::parse(id).expect("a usable profile id"),
        readonly: false,
        params: Params::Mock,
    }
}

/// What the UI knows about a mock profile before it resolves.
///
/// # Panics
///
/// If `id` is not a usable [`ProfileId`].
#[must_use]
pub fn mock_summary(id: &str) -> ProfileSummary {
    ProfileSummary {
        id: ProfileId::parse(id).expect("a usable profile id"),
        name: id.to_owned(),
        kind: DriverKind::Mock,
        color: None,
    }
}

/// A set of mock profiles, in place of a config file.
///
/// The store takes `Arc<dyn Profiles>`, so this is what stands in for
/// `sqlake-config` in a test, and what `--mock` gives the binary in place of
/// the file.
#[derive(Debug, Clone)]
pub struct MockProfiles {
    profiles: Vec<ProfileSummary>,
}

impl MockProfiles {
    /// One profile per id, in the order given.
    ///
    /// # Panics
    ///
    /// If an id is not a usable [`ProfileId`].
    #[must_use]
    pub fn new<'a>(ids: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            profiles: ids.into_iter().map(mock_summary).collect(),
        }
    }
}

impl Default for MockProfiles {
    /// The single connection M0 used to hardcode.
    fn default() -> Self {
        Self::new(["mock"])
    }
}

impl Profiles for MockProfiles {
    fn list(&self) -> Vec<ProfileSummary> {
        self.profiles.clone()
    }

    fn resolve(&self, id: &ProfileId) -> Result<ResolvedProfile, ProfileError> {
        if self.profiles.iter().any(|p| &p.id == id) {
            Ok(mock_profile(id.as_str()))
        } else {
            Err(ProfileError::new(format!("no profile called `{id}`")))
        }
    }
}

/// A two-level hierarchy, deliberately shorter than PostgreSQL's three.
pub const HIERARCHY: &[HierarchyLevel] = &[
    HierarchyLevel::new(NodeKind::Namespace, "schema"),
    HierarchyLevel::new(NodeKind::Relation, "table"),
];

/// A three-level hierarchy, for callers that need to prove they are not
/// hardcoded to the mock's own shape.
///
/// The mock serves whichever of the two it is configured with — see
/// [`MockDriver::with_capabilities`]. Shipping only [`HIERARCHY`] would mean
/// the tree still saw exactly one shape in M0, just a different one from
/// PostgreSQL's, which is the thing the short hierarchy was supposed to
/// prevent.
pub const DEEP_HIERARCHY: &[HierarchyLevel] = &[
    HierarchyLevel::new(NodeKind::Catalog, "database"),
    HierarchyLevel::new(NodeKind::Namespace, "schema"),
    HierarchyLevel::new(NodeKind::Relation, "table"),
];

/// The name of the single database the mock exposes when it is configured with
/// [`DEEP_HIERARCHY`].
pub const CATALOG_NAME: &str = "mock";

/// The default capability set. Spread it to vary one field:
/// `Capabilities { indexes: true, ..CAPABILITIES }`.
pub const CAPABILITIES: Capabilities = Capabilities {
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
///
/// Every field here exists because some surface would otherwise be written
/// blind: a spinner that is never seen, an error panel that is never rendered,
/// a retry button whose success path never runs.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// Applied to every call.
    pub latency: Duration,
    /// Connecting fails with [`DriverError::Connect`].
    ///
    /// The most common real failure by a wide margin, and the one
    /// `is_retryable` was written for.
    pub connect_fails: bool,
    /// Node paths whose expansion or preview always fails.
    pub failing_nodes: Vec<Vec<String>>,
    /// Node paths that fail the first `n` calls and then succeed.
    ///
    /// A permanent failure can only test a retry up to the point of failing
    /// again. Clearing the error, populating the children and dropping the
    /// spinner — the part a user actually sees — needs a failure that stops.
    pub flaky_nodes: Vec<(Vec<String>, u32)>,
    /// Node paths that succeed the first `n` calls and fail after that.
    ///
    /// The mirror of [`Behaviour::flaky_nodes`], and the only way to reach a
    /// failure that arrives *after* something is already on screen — a second
    /// page that does not come back, with the first page still displayed.
    pub failing_after: Vec<(Vec<String>, u32)>,
    /// Node paths that take [`Behaviour::slow_latency`] instead.
    pub slow_nodes: Vec<Vec<String>>,
    pub slow_latency: Duration,
}

/// How many times each flaky path has been asked for.
///
/// State, not configuration, so it lives on the driver rather than on
/// [`Behaviour`] — and is shared with every session the driver makes, so a
/// retry counts against the same budget as the call that failed.
type Attempts = Mutex<HashMap<Vec<String>, u32>>;

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
            ..Self::instant()
        }
    }

    /// Every path this behaviour names, for the resolution check in
    /// [`MockDriver::new`].
    fn injected_paths(&self) -> impl Iterator<Item = &Vec<String>> {
        self.failing_nodes
            .iter()
            .chain(self.slow_nodes.iter())
            .chain(self.flaky_nodes.iter().map(|(p, _)| p))
            .chain(self.failing_after.iter().map(|(p, _)| p))
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

    /// Whether this call should fail. Consumes one of the flaky budget, so it
    /// must be asked exactly once per call.
    fn fails_for(&self, path: &[String], attempts: &Attempts) -> bool {
        if Self::matches(&self.failing_nodes, path) {
            return true;
        }
        let flaky = self.flaky_nodes.iter().find(|(p, _)| p == path);
        let late = self.failing_after.iter().find(|(p, _)| p == path);
        if flaky.is_none() && late.is_none() {
            return false;
        }

        let mut attempts = attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seen = attempts.entry(path.to_vec()).or_insert(0);
        *seen += 1;

        flaky.is_some_and(|(_, times)| *seen <= *times)
            || late.is_some_and(|(_, times)| *seen > *times)
    }
}

/// Drops the catalogue segment when the mock is configured with
/// [`DEEP_HIERARCHY`], so a path resolves the same at either depth.
pub(crate) fn without_catalog(path: &[String]) -> &[String] {
    match path {
        [first, rest @ ..] if first.as_str() == CATALOG_NAME => rest,
        _ => path,
    }
}

#[derive(Debug)]
pub struct MockDriver {
    behaviour: Behaviour,
    capabilities: Capabilities,
    catalog: Arc<Catalog>,
    attempts: Arc<Attempts>,
}

impl MockDriver {
    /// # Panics
    ///
    /// If `behaviour` names a node that is not in the catalogue. Injection that
    /// silently matches nothing is worse than no injection: renaming a fixture
    /// would leave every test green while the error path it exercised stopped
    /// being exercised at all.
    #[must_use]
    pub fn new(behaviour: Behaviour) -> Self {
        let catalog = fixtures::catalog();
        for path in behaviour.injected_paths() {
            assert!(
                catalog.resolves(path),
                "{path:?} is not in the catalogue, so injecting on it does nothing"
            );
        }
        Self {
            behaviour,
            capabilities: CAPABILITIES,
            catalog: Arc::new(catalog),
            attempts: Arc::default(),
        }
    }

    /// Advertise a different capability set — a deeper hierarchy, index
    /// support, the other quote style.
    ///
    /// Without this the mock is the only driver in M0, so anything reading
    /// [`Capabilities`] is exercised against exactly one answer, and UI code
    /// that hardcodes the mock's shape passes every test.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
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
        self.capabilities
    }

    async fn connect(&self, profile: &ResolvedProfile) -> DriverResult<Box<dyn Session>> {
        // The mock needs nothing from the profile, but a profile built for
        // another driver reaching this one is a wiring mistake, and the point
        // of taking the argument is to be the thing that notices.
        if !matches!(profile.params, Params::Mock) {
            return Err(DriverError::Connect(format!(
                "mock: profile `{}` is not a mock profile",
                profile.id
            )));
        }
        self.behaviour.delay_for(&[]).await;
        if self.behaviour.connect_fails {
            return Err(DriverError::Connect(
                "mock: refused by the configured behaviour".to_owned(),
            ));
        }
        Ok(Box::new(MockSession {
            behaviour: self.behaviour.clone(),
            capabilities: self.capabilities,
            catalog: Arc::clone(&self.catalog),
            attempts: Arc::clone(&self.attempts),
        }))
    }
}

#[derive(Debug)]
pub struct MockSession {
    behaviour: Behaviour,
    capabilities: Capabilities,
    catalog: Arc<Catalog>,
    attempts: Arc<Attempts>,
}

#[async_trait]
impl Session for MockSession {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Driven by the advertised hierarchy rather than by a hardcoded two
    /// levels, so the mock serves whatever shape it claims to have.
    async fn children(&self, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
        self.behaviour.delay_for(&of.path).await;

        let Some(level) = self.capabilities.hierarchy.get(of.path.len()) else {
            // Past the last level: relations are leaves until M5 adds columns.
            return Ok(Vec::new());
        };

        // Resolved before the failure is injected. The other order lets a
        // failure be attached to a node that does not exist, which is how a
        // renamed fixture turns into a test that passes without testing.
        let children = match level.kind {
            NodeKind::Catalog => vec![TreeNode::branch(
                of.child(NodeKind::Catalog, CATALOG_NAME),
                CATALOG_NAME,
            )],

            NodeKind::Namespace => self
                .catalog
                .schemas
                .iter()
                .map(|s| TreeNode::branch(of.child(NodeKind::Namespace, s.name), s.name))
                .collect(),

            NodeKind::Relation => {
                let name = of.name().unwrap_or_default();
                let schema = self
                    .catalog
                    .schema(name)
                    .ok_or_else(|| DriverError::NotFound(format!("schema {name}")))?;
                schema
                    .tables
                    .iter()
                    .map(|t| {
                        TreeNode::relation(of.child(NodeKind::Relation, t.name), t.name, t.kind)
                    })
                    .collect()
            }

            NodeKind::Root => {
                return Err(DriverError::Unsupported(
                    "the root cannot appear inside a hierarchy".to_owned(),
                ));
            }
        };

        if self.behaviour.fails_for(&of.path, &self.attempts) {
            return Err(DriverError::Query(format!("permission denied for {of}")));
        }
        Ok(children)
    }

    async fn preview(&self, table: &TableRef, req: &PageRequest) -> DriverResult<ResultSet> {
        self.behaviour.delay_for(&table.path).await;

        let [schema, name] = without_catalog(&table.path) else {
            return Err(DriverError::NotFound(format!("{table} is not a relation")));
        };
        let fixture = self
            .catalog
            .table(schema, name)
            .ok_or_else(|| DriverError::NotFound(table.to_string()))?;

        if self.behaviour.fails_for(&table.path, &self.attempts) {
            return Err(DriverError::Query(format!(
                "relation {table} is corrupt: unexpected page header"
            )));
        }

        if let Some(sort) = req.sort {
            // A real engine answers "no such column" rather than returning the
            // rows in storage order and calling them sorted.
            if sort.column >= fixture.columns.len() {
                return Err(DriverError::Query(format!(
                    "sort column {} is out of range for {table}, which has {}",
                    sort.column,
                    fixture.columns.len()
                )));
            }
        }

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
            fixture.total_rows(),
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
            .connect(&mock_profile("mock"))
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

    #[test]
    fn the_shipped_behaviour_names_nodes_that_exist() {
        // Behaviour::fixture() is what the app actually runs against, and it
        // was the one configuration no test constructed. MockDriver::new
        // asserts every injected path resolves, so this fails loudly if a
        // fixture is renamed out from under it.
        let _ = MockDriver::new(Behaviour::fixture());
    }

    #[test]
    #[should_panic(expected = "is not in the catalogue")]
    fn injecting_on_a_node_that_does_not_exist_is_refused() {
        let _ = MockDriver::new(Behaviour {
            // The mistake this guards against: one segment containing a dot,
            // which reads correctly to a human and matches nothing.
            failing_nodes: vec![vec!["analytics.broken".to_owned()]],
            ..Behaviour::instant()
        });
    }

    #[tokio::test]
    async fn connecting_can_fail() {
        let driver = MockDriver::new(Behaviour {
            connect_fails: true,
            ..Behaviour::instant()
        });
        let err = driver.connect(&mock_profile("mock")).await.unwrap_err();
        assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_flaky_node_succeeds_once_the_retries_run_out() {
        let driver = MockDriver::new(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned()], 2)],
            ..Behaviour::instant()
        });
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);

        assert!(s.children(&node).await.is_err(), "first attempt");
        assert!(s.children(&node).await.is_err(), "second attempt");
        // The part a permanent failure cannot reach: the error clears and the
        // children arrive.
        let nodes = s.children(&node).await.unwrap();
        assert!(nodes.iter().any(|n| n.label == "users"));
    }

    #[tokio::test]
    async fn a_node_can_start_failing_after_it_has_worked() {
        let driver = MockDriver::new(Behaviour {
            failing_after: vec![(vec!["public".to_owned(), "big".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
        let table = TableRef::new(["public", "big"]);

        assert!(s.preview(&table, &PageRequest::first()).await.is_ok());
        // The second page does not come back, with the first still on screen.
        assert!(s.preview(&table, &PageRequest::first()).await.is_err());
    }

    #[tokio::test]
    async fn the_hierarchy_the_driver_advertises_is_the_one_it_serves() {
        let driver = MockDriver::new(Behaviour::instant()).with_capabilities(Capabilities {
            hierarchy: DEEP_HIERARCHY,
            ..CAPABILITIES
        });
        let s = driver.connect(&mock_profile("mock")).await.unwrap();

        let catalogs = s.children(&NodeRef::root()).await.unwrap();
        assert_eq!(catalogs.len(), 1);
        assert_eq!(catalogs[0].label, CATALOG_NAME);

        let schemas = s.children(&catalogs[0].node_ref).await.unwrap();
        assert!(schemas.iter().any(|n| n.label == "public"));

        let public = schemas.iter().find(|n| n.label == "public").unwrap();
        let tables = s.children(&public.node_ref).await.unwrap();
        assert!(tables.iter().any(|n| n.label == "users"));

        // And the relation is still a leaf, one level deeper than before.
        let users = tables.iter().find(|n| n.label == "users").unwrap();
        assert!(s.children(&users.node_ref).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_deep_path_previews_the_same_relation() {
        let driver = MockDriver::new(Behaviour::instant()).with_capabilities(Capabilities {
            hierarchy: DEEP_HIERARCHY,
            ..CAPABILITIES
        });
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
        let rs = s
            .preview(
                &TableRef::new([CATALOG_NAME, "public", "users"]),
                &PageRequest::first(),
            )
            .await
            .unwrap();
        assert_eq!(rs.column_count(), 8);
    }

    #[tokio::test]
    async fn a_relation_may_report_no_total() {
        let s = session().await;
        let rs = s
            .preview(
                &TableRef::new(["analytics", "unbounded"]),
                &PageRequest::first(),
            )
            .await
            .unwrap();
        // The case core documents as the common one for real drivers, and the
        // one every division by a total has to survive.
        assert_eq!(rs.total_rows, None);
        assert!(rs.row_count() > 0);
    }

    #[tokio::test]
    async fn sorting_by_a_column_that_does_not_exist_is_an_error() {
        let s = session().await;
        let req = PageRequest {
            offset: 0,
            limit: 10,
            // `empty` has two columns.
            sort: Some(Sort::new(7, SortDir::Asc)),
        };
        let err = s
            .preview(&TableRef::new(["public", "empty"]), &req)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_node_reports_an_error_rather_than_an_empty_list() {
        let driver = MockDriver::new(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        });
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
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
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
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
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
        let start = tokio::time::Instant::now();
        s.preview(&TableRef::new(["analytics", "slow"]), &PageRequest::first())
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_secs(2));
    }
}
