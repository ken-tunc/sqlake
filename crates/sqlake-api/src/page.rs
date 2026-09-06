//! A page of rows, shaped for a reader with a context window.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlake_app::PagedResult;

use sqlake_app::json::to_json;

/// How much of a result is worth putting in front of an agent.
///
/// A rendering decision, so it lives here rather than in `sqlake-app`, which is
/// forbidden to hold one — and applying it in the store would mean the human
/// sharing the session inherits the agent's budget. The store keeps the page it
/// fetched in full either way; this only decides how much of it is written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_rows: usize,
    pub max_columns: usize,
}

impl Budget {
    /// Lower than the TUI's page size on purpose. An agent that pulls two
    /// hundred thousand rows into its context has not read the table, it has
    /// destroyed its own working memory.
    pub const DEFAULT: Self = Self {
        max_rows: 50,
        max_columns: 40,
    };
}

impl Default for Budget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// Rows are arrays, and the columns are described once.
///
/// The obvious shape is an object per row, and it repeats every column name on
/// every row into the context window the budget above exists to protect. It
/// also collapses a relation with two columns of the same name, which `SELECT
/// a.id, b.id` produces without anybody doing anything unusual.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct Page {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Json>>,
    /// Rows written out here.
    pub returned: usize,
    /// Rows the store has fetched so far — which is not the size of the
    /// relation. `total` is that, when the driver knew it; a BigQuery preview
    /// never does.
    pub loaded: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Always stated, never left to be inferred from counting rows.
    pub truncated: bool,
    /// Named, not counted: an agent that knows a column was left out can ask
    /// for it, and one told only that "3 columns were omitted" cannot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_columns: Vec<String>,
}

impl Page {
    #[must_use]
    pub fn of(result: &PagedResult, budget: Budget) -> Self {
        let kept = result.columns().len().min(budget.max_columns);
        let omitted_columns = result.columns()[kept..]
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let columns = result.columns()[..kept]
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                type_name: c.type_name.clone(),
                nullable: c.nullable,
            })
            .collect();

        let returned = result.row_count().min(budget.max_rows);
        let rows = (0..returned)
            .map(|r| {
                (0..kept)
                    .map(|c| result.value(r, c).map_or(Json::Null, to_json))
                    .collect()
            })
            .collect();

        Self {
            columns,
            rows,
            returned,
            loaded: result.row_count(),
            total: result.total_rows(),
            truncated: returned < result.row_count(),
            omitted_columns,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlake_core::result::{Column as CoreColumn, ResultSet, Row};
    use sqlake_core::value::Value;

    use super::*;

    fn result(columns: usize, rows: usize, total: Option<u64>) -> PagedResult {
        let cols = (0..columns)
            .map(|c| CoreColumn::new(format!("c{c}"), "int", false))
            .collect();
        let rows = (0..rows)
            .map(|r| {
                Row((0..columns)
                    .map(|c| Value::Int((r * columns + c) as i64))
                    .collect())
            })
            .collect();
        PagedResult::new(&ResultSet::new(cols, rows, total))
    }

    #[test]
    fn a_row_is_an_array_and_the_columns_are_described_once() {
        let page = Page::of(&result(2, 2, Some(9)), Budget::DEFAULT);
        assert_eq!(
            page.rows,
            vec![vec![json!(0), json!(1)], vec![json!(2), json!(3)]]
        );
        assert_eq!(page.columns.len(), 2);
        assert_eq!(page.total, Some(9));
        assert!(!page.truncated);
    }

    #[test]
    fn two_columns_of_one_name_both_survive() {
        // An object per row would keep one of them. `SELECT a.id, b.id`
        // produces this without anybody doing anything unusual.
        let columns = vec![
            CoreColumn::new("id", "int", false),
            CoreColumn::new("id", "int", false),
        ];
        let rows = vec![Row(vec![Value::Int(1), Value::Int(2)])];
        let page = Page::of(
            &PagedResult::new(&ResultSet::new(columns, rows, None)),
            Budget::DEFAULT,
        );
        assert_eq!(page.rows[0], vec![json!(1), json!(2)]);
        assert_eq!(page.columns.len(), 2);
    }

    #[test]
    fn a_cut_result_says_how_much_it_left() {
        let page = Page::of(
            &result(2, 10, Some(500)),
            Budget {
                max_rows: 3,
                max_columns: 40,
            },
        );
        assert_eq!(page.returned, 3);
        assert_eq!(page.loaded, 10);
        assert_eq!(page.total, Some(500));
        assert!(page.truncated);
    }

    #[test]
    fn omitted_columns_are_named_rather_than_counted() {
        // An agent told which columns it did not see can ask for them; one told
        // that three were omitted cannot.
        let page = Page::of(
            &result(5, 1, None),
            Budget {
                max_rows: 50,
                max_columns: 2,
            },
        );
        assert_eq!(page.columns.len(), 2);
        assert_eq!(page.rows[0].len(), 2, "a row is as wide as the columns");
        assert_eq!(page.omitted_columns, ["c2", "c3", "c4"]);
    }

    #[test]
    fn a_result_that_fits_says_nothing_about_cutting() {
        let json = serde_json::to_value(Page::of(&result(1, 1, None), Budget::DEFAULT)).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object["truncated"], json!(false), "always stated");
        assert!(!object.contains_key("omitted_columns"));
        assert!(!object.contains_key("total"), "the driver did not know");
    }

    #[test]
    fn an_empty_relation_still_describes_its_columns() {
        let page = Page::of(&result(3, 0, Some(0)), Budget::DEFAULT);
        assert!(page.rows.is_empty());
        assert_eq!(page.columns.len(), 3);
        assert!(!page.truncated);
    }
}
