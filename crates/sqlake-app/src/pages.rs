//! A result set accumulated a page at a time.
//!
//! Paging, not presentation. Nothing here knows how wide a terminal is or what
//! a null looks like — that is `sqlake-tui`'s `grid` for the interactive
//! client, and the agent surface's own serialisation for the API, and the two
//! want opposite things. Collapsing a JSON document to `{2 keys}` is exactly
//! right on screen and destroys the data an agent asked for.

use std::sync::Arc;

use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::value::Value;

/// The rows of a relation as far as they have been fetched.
#[derive(Debug, Clone)]
pub struct PagedResult {
    columns: Arc<Vec<Column>>,
    /// One entry per page fetched.
    ///
    /// Appending pushes an `Arc`; the rows already held are never touched. The
    /// obvious version keeps one `Vec<Row>` and extends it, which copies every
    /// row fetched so far on every page — quadratic in the page count, run on
    /// the store task, which is the only writer there is.
    pages: Vec<Arc<Vec<Row>>>,
    rows: usize,
    total_rows: Option<u64>,
}

impl PagedResult {
    #[must_use]
    pub fn new(result: &ResultSet) -> Self {
        Self {
            columns: Arc::clone(&result.columns),
            rows: result.rows.len(),
            pages: vec![Arc::clone(&result.rows)],
            total_rows: result.total_rows,
        }
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Rows in the underlying relation, when the driver knew.
    ///
    /// `None` is the common case for a real driver, not an edge case: a
    /// BigQuery preview never provides one.
    #[must_use]
    pub fn total_rows(&self) -> Option<u64> {
        self.total_rows
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    #[must_use]
    pub fn row(&self, index: usize) -> Option<&Row> {
        let mut remaining = index;
        for page in &self.pages {
            if remaining < page.len() {
                return page.get(remaining);
            }
            remaining -= page.len();
        }
        None
    }

    #[must_use]
    pub fn value(&self, row: usize, col: usize) -> Option<&Value> {
        self.row(row).and_then(|r| r.get(col))
    }

    /// The first page, capped at `max` rows.
    ///
    /// What a front-end samples to decide anything about a column as a whole.
    /// Sampling the accumulated result instead would let a page arriving later
    /// change the answer, and a column that resized or re-aligned itself as
    /// pages landed would be unusable.
    #[must_use]
    pub fn sample(&self, max: usize) -> &[Row] {
        let first = self.pages.first().map_or(&[] as &[Row], |p| p.as_slice());
        &first[..first.len().min(max)]
    }

    /// This result with `more` appended.
    ///
    /// `None` when the two disagree about their columns, which means the
    /// relation changed underneath: DDL ran, or the driver altered its
    /// projection. Concatenating them anyway would leave the rows and the
    /// column list describing different things, and every consumer would
    /// render the difference as nulls without anyone noticing.
    #[must_use]
    pub fn append(&self, more: &ResultSet) -> Option<Self> {
        if !same_columns(&self.columns, &more.columns) {
            return None;
        }
        let mut pages = self.pages.clone();
        // An empty page carries nothing and would still be walked by every
        // lookup: `load_more` at the end of a relation can be asked for
        // indefinitely, and each empty reply would make `row` a step longer.
        if !more.rows.is_empty() {
            pages.push(Arc::clone(&more.rows));
        }
        Some(Self {
            columns: Arc::clone(&self.columns),
            rows: self.rows + more.rows.len(),
            pages,
            total_rows: more.total_rows.or(self.total_rows),
        })
    }
}

fn same_columns(a: &[Column], b: &[Column]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(range: std::ops::Range<i64>) -> ResultSet {
        ResultSet::new(
            vec![Column::new("id", "int8", false)],
            range.map(|i| Row(vec![Value::Int(i)])).collect(),
            None,
        )
    }

    #[test]
    fn appending_a_page_does_not_copy_the_rows_already_held() {
        let first = PagedResult::new(&page(0..100));
        let merged = first.append(&page(100..200)).unwrap();

        assert_eq!(merged.row_count(), 200);
        // The point of the whole arrangement. Extending a single `Vec` would
        // make every page cost every row fetched so far, on the store task.
        assert!(Arc::ptr_eq(&merged.pages[0], &first.pages[0]));
    }

    #[test]
    fn rows_read_correctly_across_a_page_boundary() {
        let merged = PagedResult::new(&page(0..3)).append(&page(3..6)).unwrap();
        for i in 0..6 {
            assert_eq!(merged.value(i, 0), Some(&Value::Int(i as i64)), "row {i}");
        }
        assert_eq!(merged.value(6, 0), None);
    }

    #[test]
    fn a_page_with_different_columns_is_refused() {
        let first = PagedResult::new(&page(0..1));
        let renamed = ResultSet::new(
            vec![Column::new("identifier", "int8", false)],
            vec![Row(vec![Value::Int(2)])],
            None,
        );
        assert!(first.append(&renamed).is_none());
    }

    #[test]
    fn a_later_page_supplies_a_total_the_first_did_not_have() {
        let first = PagedResult::new(&page(0..2));
        assert_eq!(first.total_rows(), None);

        let counted = ResultSet::new(
            vec![Column::new("id", "int8", false)],
            vec![Row(vec![Value::Int(2)])],
            Some(3),
        );
        assert_eq!(first.append(&counted).unwrap().total_rows(), Some(3));
    }

    #[test]
    fn an_empty_result_still_reports_its_columns() {
        let empty = PagedResult::new(&ResultSet::new(
            vec![Column::new("id", "int8", false)],
            Vec::new(),
            Some(0),
        ));
        assert!(empty.is_empty());
        assert_eq!(empty.columns().len(), 1);
        assert_eq!(empty.row(0), None);
    }
}
