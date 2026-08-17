//! Walking the tree: project, dataset, table.
//!
//! All three levels come from REST endpoints that are not billed. Listing what
//! is in a project costs nothing; the equivalent query against
//! `INFORMATION_SCHEMA` would be a job, and clicking a triangle must not be.

use gcp_bigquery_client::Client;
use gcp_bigquery_client::model::dataset::Dataset;
use gcp_bigquery_client::model::table_list_tables::TableListTables;
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::node::{NodeKind, NodeRef, RelationKind, TreeNode};

use crate::error::{driver_error, is_empty_dataset_list, listing_failed};

pub async fn children(client: &Client, project: &str, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    match of.kind {
        // The one project the profile named. BigQuery can read across projects
        // and the tree shows one, because a connection is to a project: the
        // billing account and the location are its, and a second project is a
        // second connection rather than a second branch.
        NodeKind::Root => Ok(vec![TreeNode::branch(
            of.child(NodeKind::Catalog, project),
            project,
        )]),
        NodeKind::Catalog => datasets(client, of).await,
        NodeKind::Namespace => tables(client, of).await,
        // A table's children are its columns, which is `describe`'s answer in
        // M5 rather than another level of tree.
        NodeKind::Relation => Ok(Vec::new()),
    }
}

async fn datasets(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    let [project] = of.path.as_slice() else {
        return Err(not_a(of, "project"));
    };
    let mut page_token = None;
    let mut nodes = Vec::new();

    loop {
        let mut options = gcp_bigquery_client::dataset::ListOptions::default();
        let first = page_token.is_none();
        if let Some(token) = page_token {
            options = options.page_token(token);
        }
        let listed = match client.dataset().list(project, options).await {
            Ok(listed) => listed,
            // An empty project is not an error, and a body that did not parse
            // is the only way it can be told apart from one — see
            // `is_empty_dataset_list`.
            //
            // Only on the first request, though. `nextPageToken` is sent when
            // there is a next page, so a page asked for by token has datasets
            // in it, and a parse failure there is a parse failure. Forgiving it
            // anywhere would end the loop and return what had been collected so
            // far: a branch missing half its datasets, with nothing on screen
            // to say so. An error the user can retry is the better half of that
            // trade, because a truncated tree cannot be noticed at all.
            Err(err) if first && is_empty_dataset_list(&err) => break,
            Err(err) => return Err(listing_failed(err)),
        };

        nodes.extend(listed.datasets.iter().map(|dataset| node(of, dataset)));
        page_token = next_page(listed.next_page_token);
        if page_token.is_none() {
            break;
        }
    }

    Ok(nodes)
}

/// The token to ask for the next page with, or `None` for "that was the last".
///
/// An empty token is read as the end rather than sent: `pageToken=` is a
/// parameter the API ignores, so it would be answered with the first page
/// again — a loop that never ends and never stops growing, on the task that
/// serialises every request this connection makes.
fn next_page(token: Option<String>) -> Option<String> {
    token.filter(|token| !token.is_empty())
}

/// A node this driver could not have produced. Reaching it is a caller's bug,
/// so what it has to carry is the path that was asked for.
fn not_a(of: &NodeRef, level: &str) -> DriverError {
    driver_error(format!("`{of}` is not a BigQuery {level}"))
}

fn node(of: &NodeRef, dataset: &Dataset) -> TreeNode {
    let name = &dataset.dataset_reference.dataset_id;
    TreeNode::branch(of.child(NodeKind::Namespace, name), name)
}

async fn tables(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    // `[project, dataset]` — the level above supplied both, and a dataset is
    // only addressable inside the project that owns it.
    let [project, dataset] = of.path.as_slice() else {
        return Err(not_a(of, "dataset"));
    };
    let mut page_token = None;
    let mut nodes = Vec::new();

    loop {
        let mut options = gcp_bigquery_client::table::ListOptions::default();
        if let Some(token) = page_token {
            options = options.page_token(token);
        }
        let listed = client
            .table()
            .list(project, dataset, options)
            .await
            .map_err(listing_failed)?;

        nodes.extend(
            listed
                .tables
                .unwrap_or_default()
                .iter()
                .map(|table| relation(of, table)),
        );
        page_token = next_page(listed.next_page_token);
        if page_token.is_none() {
            break;
        }
    }

    Ok(nodes)
}

fn relation(of: &NodeRef, table: &TableListTables) -> TreeNode {
    let name = &table.table_reference.table_id;
    TreeNode::relation(
        of.child(NodeKind::Relation, name),
        name,
        relation_kind(table.r#type.as_deref()),
    )
}

/// BigQuery's `type` as the shared model sees it.
///
/// Anything unrecognised is a table, including the `SNAPSHOT` this list does
/// not name: a type nobody has mapped is far more likely to be something rows
/// can be read from than not, and the wrong icon is a smaller failure than the
/// relation vanishing out of the tree.
fn relation_kind(kind: Option<&str>) -> RelationKind {
    match kind {
        Some("VIEW") => RelationKind::View,
        Some("MATERIALIZED_VIEW") => RelationKind::MaterializedView,
        Some("EXTERNAL") => RelationKind::External,
        _ => RelationKind::Table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_type_that_holds_rows_is_a_relation() {
        assert_eq!(relation_kind(Some("TABLE")), RelationKind::Table);
        assert_eq!(relation_kind(Some("VIEW")), RelationKind::View);
        assert_eq!(
            relation_kind(Some("MATERIALIZED_VIEW")),
            RelationKind::MaterializedView
        );
        assert_eq!(relation_kind(Some("EXTERNAL")), RelationKind::External);
        // The one the API has that this list does not, and the case of a
        // response with no `type` at all.
        assert_eq!(relation_kind(Some("SNAPSHOT")), RelationKind::Table);
        assert_eq!(relation_kind(None), RelationKind::Table);
    }

    #[test]
    fn an_empty_page_token_ends_the_listing() {
        assert_eq!(
            next_page(Some("page-2".to_owned())).as_deref(),
            Some("page-2")
        );
        assert_eq!(next_page(Some(String::new())), None);
        assert_eq!(next_page(None), None);
    }
}
