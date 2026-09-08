//! An in-memory driver used to develop and test everything else without a
//! database.
//!
//! It injects latency and failures on purpose. Without them, loading states and
//! error surfaces get written blind, and a UI that blocks is only discovered
//! once a real driver lands on top of it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{self, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlake_core::capability::{Capabilities, DriverKind, Escaping, HierarchyLevel, QuoteStyle};
use sqlake_core::detail::{ColumnDef, DetailSection, TableDetail};
use sqlake_core::driver::{Driver, DriverError, DriverResult, Session};
use sqlake_core::id::ProfileId;
use sqlake_core::node::{NodeKind, NodeRef, TableRef, TreeNode};
use sqlake_core::profile::{Params, ProfileError, ProfileSummary, Profiles, ResolvedProfile};
use sqlake_core::result::{Column, PageRequest, ResultSet, Row, Sort, SortDir};
use sqlake_core::sql::{ApprovedQuery, Estimate, Position, ValidatedSql};
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
    /// What the profiles this hands out say they allow.
    ///
    /// Here rather than on [`mock_profile`] because the thing under test is
    /// usually the store, and the store only ever sees a profile through this.
    readonly: bool,
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
            readonly: false,
        }
    }

    /// [`Self::default`]'s single profile, marked read-only.
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            readonly: true,
            ..Self::default()
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
            Ok(ResolvedProfile {
                readonly: self.readonly,
                ..mock_profile(id.as_str())
            })
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
    sortable_preview: true,
    quote_style: QuoteStyle::DoubleQuote,
    escaping: Escaping::None,
};

/// The pair BigQuery will answer, before there is a BigQuery driver to answer
/// it: a preview that costs nothing and cannot be ordered.
pub const NO_SORT: Capabilities = Capabilities {
    sortable_preview: false,
    // BigQuery's escaping too, so the scanner's backslash path is exercised
    // where the mock is the only driver.
    escaping: Escaping::Backslash,
    ..CAPABILITIES
};

/// A mock that costs a query before running it, which the default set does not.
///
/// Without this the approval path has nothing to exercise under CI, where the
/// mock is the only driver — and "the budget refused it" would be a branch no
/// test ever took.
pub const ESTIMATES: Capabilities = Capabilities {
    cost_estimate: true,
    ..CAPABILITIES
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
    /// Relations whose pages come back short of the limit with rows still to
    /// come, which is BigQuery's ordinary behaviour: `tabledata.list` caps a
    /// response at 10 MB and answers a 200-row request with as many as fit.
    ///
    /// Without it the only short page a test can produce is the last one, and
    /// "short page" and "end of the relation" are the same thing in every
    /// test — which is exactly the assumption that strands a wide table on
    /// its first screenful.
    pub short_pages: Vec<Vec<String>>,
    /// What [`Session::estimate`] answers, when the capability set says it
    /// estimates at all.
    ///
    /// Explicit rather than derived from the text: an estimate computed from
    /// the query would make every test that wants a number over the budget
    /// have to write a query of a particular length, which says nothing about
    /// what the test is for. Zero by default, so a budget only bites when a
    /// test says what it is testing.
    pub estimate_bytes: u64,
    /// How long a query takes, on top of [`Behaviour::latency`].
    ///
    /// Its own knob because `latency` applies to connecting too, and a test
    /// about cancelling a slow query would otherwise spend that time opening
    /// the connection it cancels on.
    pub query_latency: Duration,
    /// Substrings that make [`Session::execute`] fail.
    ///
    /// The message carries a line and column, because that is the shape a
    /// server's own syntax error has and the front-end has to be built against
    /// one that does.
    pub failing_sql: Vec<String>,
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
            .chain(self.short_pages.iter())
    }

    /// Whether `sql` is one of the queries configured to fail.
    fn refuses(&self, sql: &str) -> Option<&str> {
        self.failing_sql
            .iter()
            .find(|marker| sql.contains(marker.as_str()))
            .map(String::as_str)
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

/// Records that a query was unwound rather than finished.
///
/// The mock's stand-in for a driver telling its server to stop: there is no
/// server, so what it can prove is that the future was dropped mid-call, which
/// is the signal a real driver acts on.
struct Unwound(Option<Arc<AtomicU64>>);

impl Unwound {
    fn finished(&mut self) {
        self.0 = None;
    }
}

impl Drop for Unwound {
    fn drop(&mut self) {
        if let Some(count) = self.0.take() {
            count.fetch_add(1, atomic::Ordering::SeqCst);
        }
    }
}

/// A failure shaped like a server's own, with somewhere in the text to point.
///
/// The position is what T7's error marker needs, and inventing it here rather
/// than at the front-end is the rule `Capabilities` follows applied to errors:
/// the driver knows its dialect, and the UI must not have to.
fn syntax_error(sql: &str, marker: &str) -> DriverError {
    let before = sql.find(marker).map_or("", |at| &sql[..at]);
    let at = Position::new(
        u32::try_from(before.matches('\n').count() + 1).unwrap_or(1),
        u32::try_from(before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1).unwrap_or(1),
    );
    DriverError::Query {
        message: format!("mock: syntax error at or near \"{marker}\" ({at})"),
        at: Some(at),
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
    /// Queries unwound rather than finished. Shared with every session this
    /// driver makes, so a test holds the driver and asks it afterwards.
    cancelled: Arc<AtomicU64>,
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
            cancelled: Arc::default(),
        }
    }

    /// How many queries were dropped before they finished.
    #[must_use]
    pub fn cancelled(&self) -> u64 {
        self.cancelled.load(atomic::Ordering::SeqCst)
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
            cancelled: Arc::clone(&self.cancelled),
        }))
    }
}

#[derive(Debug)]
pub struct MockSession {
    behaviour: Behaviour,
    capabilities: Capabilities,
    catalog: Arc<Catalog>,
    attempts: Arc<Attempts>,
    cancelled: Arc<AtomicU64>,
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
            return Err(DriverError::query(format!("permission denied for {of}")));
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
            return Err(DriverError::query(format!(
                "relation {table} is corrupt: unexpected page header"
            )));
        }

        if let Some(sort) = req.sort {
            if !self.capabilities.sortable_preview {
                return Err(DriverError::Unsupported(format!(
                    "mock: previewing {table} cannot be ordered"
                )));
            }
            // A real engine answers "no such column" rather than returning the
            // rows in storage order and calling them sorted.
            if sort.column >= fixture.columns.len() {
                return Err(DriverError::query(format!(
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

        // Cut *after* the page is built, so the rows kept are the ones the
        // request asked for and only the tail is missing — which is what a
        // response size cap does.
        let rows = if Behaviour::matches(&self.behaviour.short_pages, &table.path) {
            let keep = rows.len().div_ceil(2);
            rows.into_iter().take(keep).collect()
        } else {
            rows
        };

        Ok(ResultSet::new(
            fixture.columns.clone(),
            rows,
            fixture.total_rows(),
        ))
    }

    /// Enough of a definition to draw one, and shaped by the capabilities this
    /// mock claims.
    ///
    /// Filling in every section regardless would make the mock the one driver
    /// where `Capabilities` and the answer disagree — and the pane that reads
    /// both would be written against a shape no real driver produces.
    async fn describe(&self, table: &TableRef) -> DriverResult<TableDetail> {
        self.behaviour.delay_for(&table.path).await;

        let [schema, name] = without_catalog(&table.path) else {
            return Err(DriverError::NotFound(format!("{table} is not a relation")));
        };
        let fixture = self
            .catalog
            .table(schema, name)
            .ok_or_else(|| DriverError::NotFound(table.to_string()))?;

        if self.behaviour.fails_for(&table.path, &self.attempts) {
            return Err(DriverError::query(format!("permission denied for {table}")));
        }

        let columns = fixture
            .columns
            .iter()
            .enumerate()
            .map(|(at, column)| ColumnDef {
                name: column.name.clone(),
                type_name: column.type_name.clone(),
                nullable: column.nullable,
                // The first column stands in for one with a default, so
                // anything drawing them has a filled cell and an empty one.
                default: (at == 0).then(|| "nextval('mock_seq')".to_owned()),
                comment: (at == 0).then(|| "the primary key".to_owned()),
            })
            .collect();

        let mut detail = TableDetail::new(table.clone(), fixture.kind, columns);
        detail.comment = Some(format!("the mock's {name}"));
        detail.stats = vec![(
            "Rows".to_owned(),
            fixture
                .total_rows()
                .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
        )];
        if self.capabilities.indexes {
            detail.sections.push(DetailSection {
                title: "Indexes".to_owned(),
                table: ResultSet::new(
                    vec![
                        Column::new("name", "text", false),
                        Column::new("definition", "text", false),
                    ],
                    vec![Row(vec![
                        Value::Text(format!("{name}_pkey")),
                        Value::Text(format!(
                            "CREATE UNIQUE INDEX {name}_pkey ON {schema}.{name} ({})",
                            fixture.columns.first().map_or("id", |c| c.name.as_str())
                        )),
                    ])],
                    Some(1),
                ),
            });
        }
        if self.capabilities.partitioning {
            detail.sections.push(DetailSection {
                title: "Partitioning".to_owned(),
                table: ResultSet::new(
                    vec![Column::new("by", "text", false)],
                    vec![Row(vec![Value::Text("none".to_owned())])],
                    Some(1),
                ),
            });
        }
        Ok(detail)
    }

    async fn estimate(&self, sql: &ValidatedSql) -> DriverResult<Estimate> {
        self.behaviour.delay_for(&[]).await;
        if let Some(marker) = self.behaviour.refuses(sql.text()) {
            return Err(syntax_error(sql.text(), marker));
        }
        // Not a failure when the capability set says it cannot: "I cannot say"
        // is the honest answer, and the caller was told to expect it.
        if !self.capabilities.cost_estimate {
            return Ok(Estimate::Unknown);
        }
        Ok(Estimate::Bytes(self.behaviour.estimate_bytes))
    }

    /// Answers with the rows of whatever relation the text names.
    ///
    /// Not a SQL engine and not pretending to be one: it looks for a
    /// `schema.table` the catalogue knows and hands back that relation, so
    /// `select * from public.users` in a test or a demo produces the rows
    /// somebody reading it would expect. A query naming nothing comes back as
    /// one row holding the text, because a mock whose answer to half the
    /// queries is an error is a mock that cannot be typed into.
    async fn execute(&self, query: &ApprovedQuery) -> DriverResult<ResultSet> {
        // Counted only if this future is dropped before the query finishes,
        // which is what cancellation *is*: nothing sends the mock a message,
        // it is unwound. Without something observable here, "the cancel
        // reached the driver" would be a claim no test could make.
        let mut running = Unwound(Some(Arc::clone(&self.cancelled)));
        self.behaviour.delay_for(&[]).await;
        tokio::time::sleep(self.behaviour.query_latency).await;
        let text = query.text();
        if let Some(marker) = self.behaviour.refuses(text) {
            // A refusal is an answer, not an abandonment: leaving it armed
            // would count every failing query as a cancelled one.
            running.finished();
            return Err(syntax_error(text, marker));
        }

        let cap = query.max_rows().map_or(usize::MAX, |n| n as usize);
        let Some((schema, table)) = self.catalog.name_in(text) else {
            running.finished();
            return Ok(ResultSet::new(
                vec![Column::new("query", "text", false)],
                vec![Row(vec![Value::Text(text.to_owned())])]
                    .into_iter()
                    .take(cap)
                    .collect(),
                Some(1),
            ));
        };
        let fixture = self
            .catalog
            .table(schema, table)
            .ok_or_else(|| DriverError::NotFound(format!("{schema}.{table}")))?;
        let rows = fixture.page(
            0,
            u32::try_from(cap.min(u32::MAX as usize)).unwrap_or(u32::MAX),
        );
        running.finished();
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
    async fn a_configuration_that_cannot_sort_refuses_rather_than_ignores() {
        let driver = MockDriver::new(Behaviour::instant()).with_capabilities(NO_SORT);
        let s = driver.connect(&mock_profile("mock")).await.unwrap();
        let table = TableRef::new(["public", "users"]);
        let req = PageRequest {
            offset: 0,
            limit: 10,
            sort: Some(Sort::new(0, SortDir::Asc)),
        };
        let err = s.preview(&table, &req).await.unwrap_err();
        assert!(matches!(err, DriverError::Unsupported(_)), "{err:?}");

        // And the same page without one is still served: what it cannot do is
        // order the rows, not read them.
        let rs = s.preview(&table, &PageRequest::first()).await.unwrap();
        assert!(rs.row_count() > 0);
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
        assert!(matches!(err, DriverError::Query { .. }), "{err:?}");
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
