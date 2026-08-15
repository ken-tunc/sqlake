//! Walking the object tree, read straight out of `pg_catalog`.
//!
//! `information_schema` is the portable way to ask these questions and the
//! slow one: it is a stack of views over the same tables, with security
//! filtering that costs a join per column. `pg_catalog` answers in one index
//! scan and knows things the standard has no place for — whether a relation is
//! a partition, whether a view is materialised.
//!
//! The queries live here as constants so that the SQL a user's server sees is
//! all in one file, readable without running the client.

use sqlake_core::driver::DriverResult;
use sqlake_core::node::{NodeKind, NodeRef, RelationKind, TreeNode};
use tokio_postgres::Client;

/// The database this connection is attached to.
///
/// PostgreSQL has no cross-database queries, so the tree shows the one
/// database the connection was opened against rather than everything on the
/// server: a node nobody can open is worse than a node that is not there.
const CURRENT_DATABASE: &str = "SELECT current_database()";

/// Schemas, minus the ones that are never worth reading.
///
/// `pg_toast` holds the out-of-line storage for other tables and `pg_temp_*`
/// is this session's own scratch space. Both are internal bookkeeping. What is
/// *not* filtered is `pg_catalog` and `information_schema`: knowing what is in
/// them is half of why anyone opens a database client.
const SCHEMAS: &str = "\
    SELECT nspname \
    FROM pg_catalog.pg_namespace \
    WHERE nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp%' \
    ORDER BY nspname";

/// Relations in one schema.
///
/// Partitions are excluded — `relispartition` — because a table partitioned by
/// day has a child per day, and a tree that lists them buries the table they
/// belong to. Which partitions exist is a fact about the parent, and belongs
/// in its definition view (M5).
const RELATIONS: &str = "\
    SELECT c.relname, c.relkind \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 \
      AND NOT c.relispartition \
      AND c.relkind = ANY($2) \
    ORDER BY c.relname";

/// The relation kinds worth showing, in `relkind` letters.
///
/// `p` is a partitioned table, which is a table with no storage of its own;
/// showing it as anything else would be a distinction without a difference to
/// someone reading rows out of it.
const RELKINDS: [&str; 5] = ["r", "p", "v", "m", "f"];

/// One level of the tree.
pub async fn children(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    match of.kind {
        NodeKind::Root => database(client, of).await,
        NodeKind::Catalog => schemas(client, of).await,
        NodeKind::Namespace => relations(client, of).await,
        // A relation's children are its columns, which is `describe`'s answer
        // in M5 rather than another level of tree.
        NodeKind::Relation => Ok(Vec::new()),
    }
}

async fn database(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    let row = client
        .query_one(CURRENT_DATABASE, &[])
        .await
        .map_err(query)?;
    let name: String = row.try_get(0).map_err(query)?;
    Ok(vec![TreeNode::branch(
        of.child(NodeKind::Catalog, &name),
        name,
    )])
}

async fn schemas(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    let rows = client.query(SCHEMAS, &[]).await.map_err(query)?;
    rows.iter()
        .map(|row| {
            let name: String = row.try_get(0).map_err(query)?;
            Ok(TreeNode::branch(of.child(NodeKind::Namespace, &name), name))
        })
        .collect()
}

async fn relations(client: &Client, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
    let schema = of.name().unwrap_or_default();
    let rows = client
        .query(RELATIONS, &[&schema, &&RELKINDS[..]])
        .await
        .map_err(query)?;

    rows.iter()
        .map(|row| {
            let name: String = row.try_get(0).map_err(query)?;
            let kind: i8 = row.try_get(1).map_err(query)?;
            Ok(TreeNode::relation(
                of.child(NodeKind::Relation, &name),
                name,
                relation_kind(kind),
            ))
        })
        .collect()
}

/// `relkind` as the shared model sees it.
///
/// Anything unrecognised is a table: a new `relkind` in a future PostgreSQL is
/// far more likely to be something rows can be read from than not, and the
/// icon being wrong is a smaller failure than the relation vanishing.
fn relation_kind(relkind: i8) -> RelationKind {
    match u8::try_from(relkind).map(char::from) {
        Ok('v') => RelationKind::View,
        Ok('m') => RelationKind::MaterializedView,
        Ok('f') => RelationKind::External,
        _ => RelationKind::Table,
    }
}

fn query(err: tokio_postgres::Error) -> sqlake_core::driver::DriverError {
    sqlake_core::driver::DriverError::Query(crate::describe(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relation_kinds_that_are_not_plain_tables() {
        assert_eq!(relation_kind(b'v' as i8), RelationKind::View);
        assert_eq!(relation_kind(b'm' as i8), RelationKind::MaterializedView);
        assert_eq!(relation_kind(b'f' as i8), RelationKind::External);
        assert_eq!(relation_kind(b'r' as i8), RelationKind::Table);
        // A partitioned table is a table: the distinction means nothing to
        // someone reading rows out of it.
        assert_eq!(relation_kind(b'p' as i8), RelationKind::Table);
        // And a kind from a server newer than this code still shows up.
        assert_eq!(relation_kind(b'?' as i8), RelationKind::Table);
    }

    #[test]
    fn the_schema_query_hides_only_the_internal_ones() {
        // `pg_catalog` and `information_schema` stay: looking at them is half
        // of what a database client is for. `pg_toast` and `pg_temp_*` are
        // storage bookkeeping and belong to nobody.
        assert!(SCHEMAS.contains("pg\\_toast%"));
        assert!(SCHEMAS.contains("pg\\_temp%"));
        assert!(!SCHEMAS.contains("information_schema"));
        assert!(!SCHEMAS.contains("'pg_catalog'"));
    }

    #[test]
    fn the_relation_query_asks_the_server_to_filter_and_sort() {
        // Both matter for a schema with thousands of relations: filtering here
        // is an index scan, and filtering afterwards is a full transfer.
        assert!(RELATIONS.contains("NOT c.relispartition"));
        assert!(RELATIONS.contains("ORDER BY c.relname"));
        // Parameters, not interpolation: a schema name is a value here.
        assert!(RELATIONS.contains("n.nspname = $1"));
    }
}
