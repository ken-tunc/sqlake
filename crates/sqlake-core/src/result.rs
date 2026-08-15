//! Result sets and the requests that produce them.

use std::sync::Arc;

use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The driver's own name for the type, shown as-is. Not interpreted.
    pub type_name: String,
    pub nullable: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, nullable: bool) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            nullable,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub Vec<Value>);

impl Row {
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.0.get(index)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Value> for Row {
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A page of results.
///
/// Both fields are `Arc` so that cloning a snapshot is a pointer copy rather
/// than a copy of every row.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    pub columns: Arc<Vec<Column>>,
    pub rows: Arc<Vec<Row>>,
    /// Total rows in the underlying relation, when known. `None` means unknown
    /// or too expensive to determine, which is the common case.
    pub total_rows: Option<u64>,
}

impl ResultSet {
    #[must_use]
    pub fn new(columns: Vec<Column>, rows: Vec<Row>, total_rows: Option<u64>) -> Self {
        Self {
            columns: Arc::new(columns),
            rows: Arc::new(rows),
            total_rows,
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new(), Vec::new(), Some(0))
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    /// The arrow drawn in a column header.
    #[must_use]
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Asc => "▲",
            Self::Desc => "▼",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort {
    /// Index into [`ResultSet::columns`].
    pub column: usize,
    pub dir: SortDir,
}

impl Sort {
    #[must_use]
    pub const fn new(column: usize, dir: SortDir) -> Self {
        Self { column, dir }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageRequest {
    pub offset: u64,
    pub limit: u32,
    pub sort: Option<Sort>,
}

impl PageRequest {
    pub const DEFAULT_LIMIT: u32 = 200;

    #[must_use]
    pub const fn first() -> Self {
        Self::first_of(Self::DEFAULT_LIMIT)
    }

    #[must_use]
    pub const fn first_of(limit: u32) -> Self {
        Self {
            offset: 0,
            limit,
            sort: None,
        }
    }

    #[must_use]
    pub const fn with_sort(mut self, sort: Option<Sort>) -> Self {
        self.sort = sort;
        self
    }

    /// The page that follows this one, keeping the same size and ordering.
    #[must_use]
    pub const fn next_page(self) -> Self {
        Self {
            offset: self.offset + self.limit as u64,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_advances_by_the_page_size() {
        let p = PageRequest::first();
        assert_eq!(p.offset, 0);
        let p = p.next_page();
        assert_eq!(p.offset, u64::from(PageRequest::DEFAULT_LIMIT));
        assert_eq!(p.limit, PageRequest::DEFAULT_LIMIT);
    }

    #[test]
    fn paging_preserves_the_sort() {
        let sort = Sort::new(2, SortDir::Desc);
        let p = PageRequest::first().with_sort(Some(sort)).next_page();
        assert_eq!(p.sort, Some(sort));
    }

    #[test]
    fn sort_direction_toggles() {
        assert_eq!(SortDir::Asc.toggled(), SortDir::Desc);
        assert_eq!(SortDir::Desc.toggled().toggled(), SortDir::Desc);
    }

    #[test]
    fn an_empty_result_set_is_distinguishable_from_an_unknown_one() {
        let empty = ResultSet::empty();
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.total_rows, Some(0));

        let unknown = ResultSet::new(vec![Column::new("a", "int", false)], Vec::new(), None);
        assert_eq!(unknown.row_count(), 0);
        assert_eq!(unknown.total_rows, None);
    }

    #[test]
    fn cloning_shares_the_rows() {
        let rs = ResultSet::new(
            vec![Column::new("a", "int", false)],
            vec![Row(vec![Value::Int(1)])],
            None,
        );
        let clone = rs.clone();
        assert!(Arc::ptr_eq(&rs.rows, &clone.rows));
    }
}
