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
//! asked for, and that sorting reverses the rows are the same everywhere.
//!
//! The last one is the reason this exists at all: a whole task's worth of SQL
//! shipped with a parameter bound as the wrong type, and every test in the
//! workspace passed, because none of them spoke to a server.

use std::sync::Arc;

use sqlake_core::capability::Capabilities;
use sqlake_core::driver::{Driver, DriverError, Session};
use sqlake_core::node::{NodeKind, NodeRef, TableRef};
use sqlake_core::profile::ResolvedProfile;
use sqlake_core::result::{PageRequest, ResultSet, Sort, SortDir};
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
    paging_moves_the_window(&*session, &relation, kind).await;
    sorting_reverses_the_rows(&*session, &relation, kind).await;
    a_page_past_the_end_still_has_columns(&*session, &relation, kind).await;
    a_relation_that_is_not_there_is_an_error(&*session, subject, kind).await;

    session.close().await;
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
async fn paging_moves_the_window(session: &dyn Session, relation: &TableRef, kind: &str) {
    let sorted = |offset| page_of(2, offset).with_sort(Some(Sort::new(0, SortDir::Asc)));
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

/// Naming something that is not there fails, and does so as an error.
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
