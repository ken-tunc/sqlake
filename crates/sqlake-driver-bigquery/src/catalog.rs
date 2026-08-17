//! Walking the tree: project, dataset, table.
//!
//! All three levels come from REST endpoints that are not billed. Listing what
//! is in a project costs nothing; the equivalent query against
//! `INFORMATION_SCHEMA` would be a job, and clicking a triangle must not be.

use gcp_bigquery_client::Client;
use gcp_bigquery_client::error::BQError;
use gcp_bigquery_client::model::dataset::Dataset;
use gcp_bigquery_client::model::table_list_tables::TableListTables;
use sqlake_core::driver::DriverResult;
use sqlake_core::node::{NodeKind, NodeRef, RelationKind, TreeNode};

use crate::error::{is_empty_dataset_list, listing_failed};

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
    let project = of.name().unwrap_or_default();
    let mut page_token = None;
    let mut nodes = Vec::new();

    loop {
        let mut options = gcp_bigquery_client::dataset::ListOptions::default();
        if let Some(token) = page_token {
            options = options.page_token(token);
        }
        let listed = match client.dataset().list(project, options).await {
            Ok(listed) => listed,
            // An empty project is not an error, and this is the only way it
            // can be told apart from one — see `is_empty_dataset_list`.
            Err(err) if is_empty_dataset_list(&err) => break,
            Err(err) => return Err(listing_failed(err)),
        };

        nodes.extend(listed.datasets.iter().map(|dataset| node(of, dataset)));
        page_token = listed.next_page_token;
        if page_token.is_none() {
            break;
        }
    }

    Ok(nodes)
}

fn node(of: &NodeRef, dataset: &Dataset) -> TreeNode {
    let name = &dataset.dataset_reference.dataset_id;
    TreeNode::branch(of.child(NodeKind::Namespace, name), name)
}

async fn tables(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    // `[project, dataset]` — the level above supplied both, and a dataset is
    // only addressable inside the project that owns it.
    let [project, dataset] = of.path.as_slice() else {
        return Err(listing_failed(BQError::NoDataAvailable));
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
        page_token = listed.next_page_token;
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
/// Anything unrecognised is a table, including the `SNAPSHOT` and `MODEL` this
/// list does not name: a type nobody has mapped is far more likely to be
/// something rows can be read from than not, and the wrong icon is a smaller
/// failure than the relation vanishing out of the tree.
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
        // The two the API has that this list does not, and the case of a
        // response with no `type` at all.
        assert_eq!(relation_kind(Some("SNAPSHOT")), RelationKind::Table);
        assert_eq!(relation_kind(None), RelationKind::Table);
    }
}
