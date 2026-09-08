//! One set of cases, run against every driver.
//!
//! A driver is not "correct" on its own — it is correct if the application
//! layer can drive it the same way it drives the others. That is a property of
//! the pair, so it cannot be tested inside either crate: the mock would be
//! testing its own fixture and PostgreSQL would be testing its own SQL, and
//! the two could drift apart while both stayed green.
//!
//! The cases below are deliberately about *shape* rather than content. What a
//! table is called and what is in it differ per driver; that `children` stops
//! where `Capabilities` says it stops, that a page is never longer than it was
//! asked for, and that sorting does what `Capabilities` claims it does are the
//! same everywhere.
//!
//! The last one is the reason this exists at all: a whole task's worth of SQL
//! shipped with a parameter bound as the wrong type, and every test in the
//! workspace passed, because none of them spoke to a server.

use std::future::Future as _;
use std::sync::Arc;
use std::task::{Context, Waker};

use sqlake_core::capability::Capabilities;
use sqlake_core::driver::{Driver, DriverError, Session};
use sqlake_core::node::{NodeKind, NodeRef, TableRef};
use sqlake_core::profile::ResolvedProfile;
use sqlake_core::result::{PageRequest, ResultSet, Sort, SortDir};
use sqlake_core::sql::{Access, ApprovedQuery, Estimate, RawSql, ValidatedSql};
use sqlake_core::value::Value;

/// What a driver has to supply to be put through the suite.
#[derive(Debug)]
pub struct Subject {
    pub driver: Arc<dyn Driver>,
    pub profile: ResolvedProfile,
    /// A relation with at least four rows and at least two columns, whose
    /// first column sorts distinctly — the suite pages through it and reverses
    /// it, and neither says anything about a relation with three equal rows.
    ///
    /// At most [`WHOLE`] rows, because one case asks for the whole of it.
    pub relation: TableRef,
    /// A relation that is not there. Same shape as `relation`, so the failure
    /// is about the name rather than about the path having the wrong depth.
    pub missing: TableRef,
    /// A statement in this driver's own dialect returning at least one row and
    /// one column.
    ///
    /// Supplied rather than written here: the point of the suite is that
    /// `sqlake-app` drives every driver the same way, not that the three speak
    /// one SQL. A statement over `relation` is the natural choice, and quoting
    /// differs between them.
    pub query: String,
    /// A statement the server refuses.
    ///
    /// The failure has to be the *server's*, so a driver that never sent it —
    /// or that reported a refusal as an empty result — is caught.
    pub broken_query: String,
}

/// Every case, in order. Panics with the case's name on the first failure.
///
/// # Panics
///
/// On any conformance failure, which is the point: this is called from a test.
pub async fn run(subject: &Subject) {
    let kind = subject.driver.kind().as_str();
    let session = subject
        .driver
        .connect(&subject.profile)
        .await
        .unwrap_or_else(|err| panic!("{kind}: connecting: {err}"));

    let capabilities = session.capabilities();
    tree_stops_where_capabilities_say(&*session, capabilities, kind).await;
    let relation = relation_is_reachable_by_walking(&*session, subject, kind).await;
    a_page_is_never_longer_than_it_was_asked_for(&*session, &relation, kind).await;
    paging_moves_the_window(&*session, &relation, capabilities, kind).await;
    sorting_does_what_is_claimed(&*session, &relation, capabilities, kind).await;
    a_page_past_the_end_still_has_columns(&*session, &relation, kind).await;
    a_relation_that_is_not_there_is_an_error(&*session, subject, kind).await;
    estimating_answers_what_the_capability_claims(&*session, subject, capabilities, kind).await;
    a_query_comes_back_with_its_columns(&*session, subject, kind).await;
    a_row_cap_is_honoured(&*session, subject, kind).await;
    a_statement_the_server_refuses_is_an_error(&*session, subject, kind).await;
    a_connection_answers_again_after_a_query_is_abandoned(&*session, subject, kind).await;
    a_relation_describes_itself(&*session, subject, capabilities, kind).await;
    a_relation_that_is_not_there_cannot_be_described(&*session, subject, kind).await;

    session.close().await;
}

/// One statement, or a panic naming the case — the suite's own inputs being
/// wrong is not something to discover as a confusing failure three cases later.
fn validated(text: &str, capabilities: Capabilities, kind: &str, what: &str) -> ValidatedSql {
    ValidatedSql::parse(&RawSql::new(text), capabilities.escaping)
        .unwrap_or_else(|err| panic!("{kind}: the suite's {what} is not one statement: {err}"))
}

/// Estimate and approve, which is the only way to reach [`Session::execute`].
async fn approved(
    session: &dyn Session,
    text: &str,
    max_rows: Option<u32>,
    kind: &str,
    what: &str,
) -> ApprovedQuery {
    let sql = validated(text, session.capabilities(), kind, what);
    let estimate = session
        .estimate(&sql)
        .await
        .unwrap_or_else(|err| panic!("{kind}: estimating the {what}: {err}"));
    // No budget: the suite is about the driver answering, not about the policy
    // over it, and a byte threshold would make the case pass or fail on how
    // big somebody's fixture table happens to be.
    ApprovedQuery::within(sql, max_rows, estimate, None, Access::ReadWrite)
        .unwrap_or_else(|why| panic!("{kind}: {why}"))
}

/// `cost_estimate` is a promise, and this is where it is kept.
///
/// A driver claiming to estimate and answering [`Estimate::Unknown`] would put
/// a number-less dialog in front of somebody about to be billed; one that does
/// not claim it and answers a number is inventing one.
async fn estimating_answers_what_the_capability_claims(
    session: &dyn Session,
    subject: &Subject,
    capabilities: Capabilities,
    kind: &str,
) {
    let sql = validated(&subject.query, capabilities, kind, "query");
    let estimate = session
        .estimate(&sql)
        .await
        .unwrap_or_else(|err| panic!("{kind}: estimating: {err}"));

    if capabilities.cost_estimate {
        assert_ne!(
            estimate,
            Estimate::Unknown,
            "{kind}: claims to estimate and then does not"
        );
    } else {
        assert_eq!(
            estimate,
            Estimate::Unknown,
            "{kind}: does not claim to estimate and answered anyway"
        );
    }
}

/// A query answers with the columns of its result, even before any row is read.
async fn a_query_comes_back_with_its_columns(session: &dyn Session, subject: &Subject, kind: &str) {
    let query = approved(session, &subject.query, None, kind, "query").await;
    let result = session
        .execute(&query)
        .await
        .unwrap_or_else(|err| panic!("{kind}: running the query: {err}"));

    assert!(
        result.column_count() > 0,
        "{kind}: a result with no columns draws nothing at all, which reads as a failure"
    );
    assert!(
        result.row_count() > 0,
        "{kind}: the suite's query is supposed to match something"
    );
    for row in result.rows.iter() {
        assert_eq!(
            row.len(),
            result.column_count(),
            "{kind}: a row that does not line up under the headers"
        );
    }
}

/// The cap is on the fetch, and the statement is not rewritten to carry it.
async fn a_row_cap_is_honoured(session: &dyn Session, subject: &Subject, kind: &str) {
    let query = approved(session, &subject.query, Some(1), kind, "query").await;
    assert_eq!(query.text(), subject.query.trim_end_matches(';').trim());

    let result = session
        .execute(&query)
        .await
        .unwrap_or_else(|err| panic!("{kind}: running the capped query: {err}"));
    assert!(
        result.row_count() <= 1,
        "{kind}: asked for one row and got {}",
        result.row_count()
    );
}

/// A statement the server refuses comes back as an error rather than as
/// nothing.
///
/// The case worth having: a driver that reported a refusal as an empty result
/// would look like a query that matched no rows, and somebody would go looking
/// at their `WHERE` clause.
async fn a_statement_the_server_refuses_is_an_error(
    session: &dyn Session,
    subject: &Subject,
    kind: &str,
) {
    let sql = validated(
        &subject.broken_query,
        session.capabilities(),
        kind,
        "broken query",
    );
    // Either half may be where it is refused: PostgreSQL plans it during the
    // estimate, and a driver that estimates nothing only finds out on the way
    // in. Both are the server saying no, which is what this is about.
    let Ok(estimate) = session.estimate(&sql).await else {
        return;
    };
    let query = ApprovedQuery::within(sql, None, estimate, None, Access::ReadWrite)
        .unwrap_or_else(|why| panic!("{kind}: {why}"));
    let err = session.execute(&query).await.err().unwrap_or_else(|| {
        panic!("{kind}: the server was sent something it cannot run and said nothing")
    });
    assert!(
        !matches!(err, DriverError::Unsupported(_)),
        "{kind}: reported the server's refusal as the driver not supporting it: {err}"
    );

    // Where, in lines and columns, whoever said it. A driver that passed the
    // server's own offset through — or its `[3:15]` — would make the front-end
    // ask which server this was, which is the one thing it must not have to.
    let DriverError::Query { at: Some(at), .. } = err else {
        panic!("{kind}: refused a statement without saying where: {err}");
    };
    let lines = u32::try_from(subject.broken_query.lines().count()).unwrap_or(u32::MAX);
    assert!(
        (1..=lines).contains(&at.line) && at.column >= 1,
        "{kind}: {at} is not inside a statement of {lines} line(s)"
    );
}

/// Giving up on a query leaves the connection usable.
///
/// The claim every driver has to keep, whatever [`Capabilities::cancel`] says
/// about the server: dropping a call must not leave the session half-way
/// through one. A driver that wrote a request and did not read its answer
/// leaves the next caller reading somebody else's rows.
async fn a_connection_answers_again_after_a_query_is_abandoned(
    session: &dyn Session,
    subject: &Subject,
    kind: &str,
) {
    let query = approved(session, &subject.query, None, kind, "query").await;
    // Polled once and dropped, which is what cancelling does one layer up.
    // By hand rather than with a timeout, so this needs no runtime of its own
    // — and so "started" means started rather than "did not finish in time".
    {
        let mut running = std::pin::pin!(session.execute(&query));
        let mut cx = Context::from_waker(Waker::noop());
        // A driver that answered on the first poll had nothing to abandon,
        // which is not a failure — the case is about what a driver that did
        // start leaves behind.
        let _ = running.as_mut().poll(&mut cx);
    }

    let again = approved(session, &subject.query, None, kind, "query").await;
    session.execute(&again).await.unwrap_or_else(|err| {
        panic!("{kind}: the connection did not survive an abandoned query: {err}")
    });
}

/// A relation says what it is, and agrees with the page about its columns.
///
/// The agreement is the case worth having. Both calls answer about the same
/// relation, and a driver whose definition disagreed with its own preview
/// would make "is this column nullable" depend on which pane you opened.
async fn a_relation_describes_itself(
    session: &dyn Session,
    subject: &Subject,
    capabilities: Capabilities,
    kind: &str,
) {
    let detail = session
        .describe(&subject.relation)
        .await
        .unwrap_or_else(|err| panic!("{kind}: describing {}: {err}", subject.relation));

    assert_eq!(
        detail.table, subject.relation,
        "{kind}: described another relation"
    );
    assert!(
        !detail.columns.is_empty(),
        "{kind}: a relation with rows in it has columns"
    );

    let page = session
        .preview(&subject.relation, &PageRequest::first())
        .await
        .unwrap_or_else(|err| panic!("{kind}: previewing: {err}"));
    let described: Vec<&str> = detail.columns.iter().map(|c| c.name.as_str()).collect();
    let previewed: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        described, previewed,
        "{kind}: the definition and the page disagree about the columns"
    );

    // A section is a claim `Capabilities` has to have made. The other
    // direction is not checked: a capability with no section is a relation
    // that has none of that thing, which is ordinary — a view has no
    // partitioning on a driver that partitions.
    for section in &detail.sections {
        assert!(
            !section.title.trim().is_empty(),
            "{kind}: a section nobody can name"
        );
        let promised = match section.title.as_str() {
            "Indexes" => capabilities.indexes,
            "Triggers" => capabilities.triggers,
            "Constraints" => capabilities.constraints,
            "Partitioning" => capabilities.partitioning,
            "Clustering" => capabilities.clustering,
            // A driver is free to answer something this suite has no flag
            // for; what it may not do is answer one it said it had not got.
            _ => true,
        };
        assert!(
            promised,
            "{kind}: answered a `{}` section it says it does not have",
            section.title
        );
    }
}

/// Describing something that is not there is an error, not an empty table.
///
/// An empty definition is what a relation with no columns looks like, and a
/// caller told that about a name it got wrong goes looking for the wrong
/// thing.
async fn a_relation_that_is_not_there_cannot_be_described(
    session: &dyn Session,
    subject: &Subject,
    kind: &str,
) {
    let err = session
        .describe(&subject.missing)
        .await
        .err()
        .unwrap_or_else(|| panic!("{kind}: described a relation that is not there"));
    assert!(
        !matches!(err, DriverError::Unsupported(_)),
        "{kind}: reported a missing relation as something it cannot do: {err}"
    );
}

/// The tree has exactly as many levels as [`Capabilities::hierarchy`] claims.
///
/// A driver that reports three levels and returns children at a fourth would
/// have the tree drawing nodes the UI has no label for; one that stops early
/// leaves a branch that never opens.
async fn tree_stops_where_capabilities_say(
    session: &dyn Session,
    capabilities: Capabilities,
    kind: &str,
) {
    let mut node = NodeRef::root();
    for (level, expected) in capabilities.hierarchy.iter().enumerate() {
        let children = session
            .children(&node)
            .await
            .unwrap_or_else(|err| panic!("{kind}: children at level {level}: {err}"));
        assert!(
            !children.is_empty(),
            "{kind}: level {level} ({}) is empty, so the tree cannot be walked",
            expected.label
        );
        assert_eq!(
            children[0].node_ref.kind, expected.kind,
            "{kind}: level {level} answers with the wrong kind of node"
        );
        node = children[0].node_ref.clone();
    }

    // One past the last level. Relations are leaves until `describe` arrives.
    let past_the_end = session
        .children(&node)
        .await
        .unwrap_or_else(|err| panic!("{kind}: children past the last level: {err}"));
    assert!(
        past_the_end.is_empty(),
        "{kind}: the hierarchy says {} levels and the tree has more",
        capabilities.depth()
    );
}

/// The relation the subject named can be found by walking, not just by
/// constructing a path.
///
/// A driver whose `children` returns paths that its own `preview` does not
/// accept is broken in a way neither half can see alone.
async fn relation_is_reachable_by_walking(
    session: &dyn Session,
    subject: &Subject,
    kind: &str,
) -> TableRef {
    let parent = NodeRef::new(
        NodeKind::Namespace,
        subject.relation.path[..subject.relation.path.len() - 1].to_vec(),
    );
    let children = session
        .children(&parent)
        .await
        .unwrap_or_else(|err| panic!("{kind}: children of {parent}: {err}"));

    let found = children
        .iter()
        .find(|child| child.node_ref.path == subject.relation.path)
        .unwrap_or_else(|| {
            panic!(
                "{kind}: {} is not among the children of {parent}",
                subject.relation
            )
        });
    assert!(
        found.relation_kind.is_some(),
        "{kind}: {} came back without a relation kind",
        subject.relation
    );
    found.node_ref.as_table().expect("a relation node")
}

async fn a_page_is_never_longer_than_it_was_asked_for(
    session: &dyn Session,
    relation: &TableRef,
    kind: &str,
) {
    let page = fetch(session, relation, &page_of(2, 0), kind).await;
    assert!(
        page.row_count() <= 2,
        "{kind}: asked for 2 rows and got {}",
        page.row_count()
    );
    assert!(
        page.column_count() >= 2,
        "{kind}: a relation with two columns came back with {}",
        page.column_count()
    );
    assert!(
        page.rows.iter().all(|row| row.len() == page.column_count()),
        "{kind}: a row has a different number of values than there are columns"
    );
}

/// The second page is not the first page again.
///
/// Getting `OFFSET` wrong is invisible in a single page and shows up as a grid
/// that scrolls for ever through the same rows.
async fn paging_moves_the_window(
    session: &dyn Session,
    relation: &TableRef,
    capabilities: Capabilities,
    kind: &str,
) {
    // Ordered where that is allowed, so the two pages differ by construction
    // rather than by luck. Where it is not, the driver's own page order is all
    // there is to hold them apart.
    let sort = capabilities
        .sortable_preview
        .then(|| Sort::new(0, SortDir::Asc));
    let sorted = |offset| page_of(2, offset).with_sort(sort);
    let first = fetch(session, relation, &sorted(0), kind).await;
    let second = fetch(session, relation, &sorted(2), kind).await;

    assert!(
        !second.rows.is_empty(),
        "{kind}: the subject relation needs at least four rows"
    );
    assert_ne!(
        first.rows, second.rows,
        "{kind}: the second page is the first page again"
    );
}

/// Whichever of the two things a driver claims about sorting, it does.
///
/// A driver that cannot sort is not excused from the case, it answers the other
/// half of it. Skipping instead would leave `sortable_preview: false` asserting
/// nothing at all — and that is the state a driver drifts into when the flag is
/// set to get a test off its back.
async fn sorting_does_what_is_claimed(
    session: &dyn Session,
    relation: &TableRef,
    capabilities: Capabilities,
    kind: &str,
) {
    if capabilities.sortable_preview {
        sorting_reverses_the_rows(session, relation, kind).await;
    } else {
        sorting_is_refused(session, relation, kind).await;
    }
}

async fn sorting_is_refused(session: &dyn Session, relation: &TableRef, kind: &str) {
    let request = page_of(4, 0).with_sort(Some(Sort::new(0, SortDir::Asc)));
    let err = session
        .preview(relation, &request)
        .await
        .err()
        .unwrap_or_else(|| panic!("{kind}: a sort it does not claim came back with rows"));

    // `Unsupported` and nothing else: the others all mean the request was
    // reasonable and something went wrong, and a caller acting on that would
    // retry a sort that can never work.
    assert!(
        matches!(err, DriverError::Unsupported(_)),
        "{kind}: refusing a sort reported as something retryable: {err}"
    );
}

/// Ascending and descending are opposites — over the *whole* relation.
///
/// The first version of this asked for four rows of each and compared them,
/// which is only true of a table with four rows in it: the first page
/// ascending is the smallest four and the first page descending is the largest
/// four, and on the mock's fifty-row fixture those are different rows
/// entirely. `Value` has no ordering to check a page against, so the property
/// that is left is reversal of everything.
async fn sorting_reverses_the_rows(session: &dyn Session, relation: &TableRef, kind: &str) {
    let sorted = |dir| page_of(WHOLE, 0).with_sort(Some(Sort::new(0, dir)));
    let ascending = fetch(session, relation, &sorted(SortDir::Asc), kind).await;
    let descending = fetch(session, relation, &sorted(SortDir::Desc), kind).await;

    let first: Vec<&Value> = ascending.rows.iter().filter_map(|r| r.get(0)).collect();
    let last: Vec<&Value> = descending.rows.iter().filter_map(|r| r.get(0)).collect();
    assert_eq!(
        first.len(),
        last.len(),
        "{kind}: the two sorts differ in size"
    );
    assert!(
        first.len() >= 4,
        "{kind}: too few rows to tell a sort apart"
    );
    assert!(
        first.len() < WHOLE as usize,
        "{kind}: the subject relation has more than {WHOLE} rows, so this is a page and not the whole of it"
    );

    let reversed: Vec<&Value> = last.into_iter().rev().collect();
    assert_eq!(
        first, reversed,
        "{kind}: descending is not the reverse of ascending"
    );
}

/// An offset past the last row is an empty page, not a shapeless one.
///
/// The grid draws its header from the columns, so a result with none of them
/// looks like a failed query rather than like the end of a table.
async fn a_page_past_the_end_still_has_columns(
    session: &dyn Session,
    relation: &TableRef,
    kind: &str,
) {
    let page = fetch(session, relation, &page_of(10, 100_000), kind).await;
    assert_eq!(page.row_count(), 0, "{kind}: rows past the end of a table");
    assert!(
        page.column_count() > 0,
        "{kind}: an empty page came back with no columns at all"
    );
}

async fn a_relation_that_is_not_there_is_an_error(
    session: &dyn Session,
    subject: &Subject,
    kind: &str,
) {
    let err = session
        .preview(&subject.missing, &PageRequest::first())
        .await
        .err()
        .unwrap_or_else(|| panic!("{kind}: previewing {} succeeded", subject.missing));

    // Not `Unsupported`: that means the caller asked for something the driver
    // does not do, and reading a table is not that.
    assert!(
        !matches!(err, DriverError::Unsupported(_)),
        "{kind}: a missing relation reported as unsupported: {err}"
    );
    assert!(
        !err.to_string().is_empty(),
        "{kind}: a missing relation failed without saying anything"
    );
}

/// Bigger than the subject relation, so a page of this size is all of it.
pub const WHOLE: u32 = 1000;

fn page_of(limit: u32, offset: u64) -> PageRequest {
    PageRequest {
        offset,
        limit,
        sort: None,
    }
}

async fn fetch(
    session: &dyn Session,
    relation: &TableRef,
    request: &PageRequest,
    kind: &str,
) -> ResultSet {
    session
        .preview(relation, request)
        .await
        .unwrap_or_else(|err| panic!("{kind}: previewing {relation} at {}: {err}", request.offset))
}
