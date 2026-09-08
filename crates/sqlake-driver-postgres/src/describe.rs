//! What a relation is, out of `pg_catalog`.
//!
//! One query per question, and each of them here as a constant so the SQL a
//! user's server sees is readable without running the client — the same rule
//! [`crate::catalog`] follows and for the same reason.
//!
//! Columns come from `pg_attribute` rather than from a prepared statement.
//! `preview` reads its columns off a result and marks them all nullable
//! because the wire protocol does not carry the answer; this is the call that
//! knows, and a `NOT NULL` nobody is shown is a constraint they do not know
//! they have.

use sqlake_core::capability::QuoteStyle;
use sqlake_core::detail::{ColumnDef, Ddl, DetailSection, TableDetail};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::ident::Ident;
use sqlake_core::node::{RelationKind, TableRef};
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::value::Value;
use tokio_postgres::Client;

/// The relation itself: what kind it is, and its comment.
///
/// Matched on `nspname` and `relname` rather than resolved through the search
/// path: the caller already names the schema, so `to_regclass` would only add
/// a way for a name to resolve to a relation somewhere else.
const RELATION: &str = "\
    SELECT c.relkind, \
           obj_description(c.oid, 'pg_class'), \
           CASE WHEN c.relkind IN ('v', 'm') \
                THEN pg_catalog.pg_get_viewdef(c.oid, true) END \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2";

/// A generated or identity column has no default, whatever `pg_attrdef` holds
/// for it. `pg_get_expr` on a `GENERATED ALWAYS AS (…) STORED` column returns
/// the generation expression, and writing that out as `DEFAULT <expr>` is a
/// statement that runs and produces a column which is neither generated nor an
/// identity — a lie rather than a gap. Omitted here so the definition and the
/// DDL are both merely incomplete about it, which is what §D4's list says.
///
/// `attnum > 0` skips the system columns — `ctid`, `xmin` and the rest — which
/// exist on every table and are nobody's schema. `NOT attisdropped` skips
/// columns a `DROP COLUMN` left behind: PostgreSQL keeps the slot, and showing
/// it would show a column called `........pg.dropped.1........`.
const COLUMNS: &str = "\
    SELECT a.attname, \
           pg_catalog.format_type(a.atttypid, a.atttypmod), \
           NOT a.attnotnull, \
           CASE WHEN a.attgenerated = '' AND a.attidentity = '' \
                THEN pg_catalog.pg_get_expr(d.adbin, d.adrelid) END, \
           col_description(a.attrelid, a.attnum) \
    FROM pg_catalog.pg_attribute a \
    JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
    WHERE n.nspname = $1 AND c.relname = $2 \
      AND a.attnum > 0 AND NOT a.attisdropped \
    ORDER BY a.attnum";

/// One row per index, with the statement that would recreate it.
///
/// `pg_get_indexdef` rather than the columns and opclasses reassembled here:
/// it is what `\d` prints, and it already knows about expressions, partial
/// indexes, operator classes and `INCLUDE` — every one of which a hand-built
/// description would get subtly wrong.
const INDEXES: &str = "\
    SELECT ic.relname, \
           pg_catalog.pg_get_indexdef(i.indexrelid), \
           i.indisprimary, \
           i.indisunique \
    FROM pg_catalog.pg_index i \
    JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
    JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 \
    ORDER BY i.indisprimary DESC, ic.relname";

/// Triggers, without the ones nobody wrote.
///
/// `tgisinternal` is set on the triggers PostgreSQL creates to enforce foreign
/// keys — three per constraint — and listing them would bury a user's own
/// under bookkeeping they cannot edit and did not ask for. They are already
/// visible as the constraint they belong to.
const TRIGGERS: &str = "\
    SELECT t.tgname, pg_catalog.pg_get_triggerdef(t.oid) \
    FROM pg_catalog.pg_trigger t \
    JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal \
    ORDER BY t.tgname";

/// Constraints, including the ones inherited from a parent table.
///
/// `contype` is a one-byte code; it is turned into a word here rather than
/// shown raw, because `c` and `f` are not something to make a reader look up.
///
/// `contype <> 'n'` is what keeps this section about constraints. PostgreSQL
/// 18 gives every `NOT NULL` column a `pg_constraint` row of its own, so a
/// forty-column table would answer with forty rows saying what the column list
/// above already says — and `psql \d`, which is what people compare this
/// against, does not list them here either.
const CONSTRAINTS: &str = "\
    SELECT con.conname, con.contype, pg_catalog.pg_get_constraintdef(con.oid) \
    FROM pg_catalog.pg_constraint con \
    JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 AND con.contype <> 'n' \
    ORDER BY con.contype, con.conname";

/// How the table is partitioned, and what its partitions are.
///
/// Two questions in one query because the answer to the second is meaningless
/// without the first: a list of children under a table that is not partitioned
/// is inheritance, which is a different thing.
///
/// `pg_get_expr(relpartbound)` is the child's bound — `FOR VALUES FROM … TO …`
/// — which is the fact somebody opening this pane is looking for.
const PARTITIONS: &str = "\
    SELECT pg_catalog.pg_get_partkeydef(c.oid), \
           child.relname, \
           pg_catalog.pg_get_expr(child.relpartbound, child.oid) \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_inherits inh ON inh.inhparent = c.oid \
    LEFT JOIN pg_catalog.pg_class child ON child.oid = inh.inhrelid \
    WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'p' \
    ORDER BY child.relname";

pub async fn describe(
    client: &Client,
    database: &str,
    table: &TableRef,
) -> DriverResult<TableDetail> {
    let (schema, name) = split(database, table)?;

    // Together: neither answer feeds the other, and a definition is two round
    // trips either way.
    let names: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&schema, &name];
    let (relation, columns) = tokio::join!(
        client.query_opt(RELATION, &names),
        client.query(COLUMNS, &names),
    );
    let relation = relation
        .map_err(query)?
        .ok_or_else(|| DriverError::NotFound(table.to_string()))?;
    let columns = columns.map_err(query)?;

    let relkind: i8 = relation.get(0);
    let mut detail = TableDetail::new(
        table.clone(),
        crate::catalog::relation_kind(relkind),
        columns
            .iter()
            .map(|row| ColumnDef {
                name: row.get(0),
                // `format_type` is what `\d` prints: `character varying(20)`
                // rather than `varchar` and a modifier to reassemble.
                type_name: row.get(1),
                nullable: row.get(2),
                default: row.get(3),
                comment: row.get(4),
            })
            .collect(),
    );
    detail.comment = relation.get(1);
    let view_body: Option<String> = relation.get(2);

    // Four more, together. Each is one index scan and none feeds another, so
    // the four of them cost one round trip rather than four. They wait for the
    // relation because a name that is not there is a refusal, not four empty
    // sections.
    let (indexes, triggers, constraints, partitions) = tokio::join!(
        client.query(INDEXES, &names),
        client.query(TRIGGERS, &names),
        client.query(CONSTRAINTS, &names),
        client.query(PARTITIONS, &names),
    );
    // A section with no rows is left out rather than shown empty: an "Indexes"
    // heading over nothing reads as a table that has none, which is true, and
    // as a driver that could not ask, which is not — and only one of those is
    // worth a heading.
    for section in [
        indexes_of(&indexes.map_err(query)?),
        triggers_of(&triggers.map_err(query)?),
        constraints_of(&constraints.map_err(query)?),
        partitioning_of(&partitions.map_err(query)?),
    ]
    .into_iter()
    .flatten()
    {
        detail.sections.push(section);
    }
    if has_ddl(relkind) {
        detail.ddl = ddl(&schema, &name, &detail, view_body.as_deref());
    }
    Ok(detail)
}

/// Whether a statement can be built for this `relkind` at all.
///
/// On the letter rather than on [`RelationKind`]: `relation_kind` calls
/// everything it does not recognise a table, so an index, a sequence or a
/// composite type would otherwise come back as a `CREATE TABLE` built out of
/// the columns `pg_attribute` keeps for it.
fn has_ddl(relkind: i8) -> bool {
    matches!(
        u8::try_from(relkind).map(char::from),
        Ok('r' | 'p' | 'v' | 'm')
    )
}

/// The statement that would recreate this relation, built from the catalogue.
///
/// PostgreSQL offers none: there is no `SHOW CREATE TABLE`, and `pg_dump` is a
/// subprocess this client will not spawn. So this is assembled from what the
/// sections above already fetched, which is also why it costs no extra round
/// trip.
///
/// **What it covers**: columns with their types, nullability and defaults;
/// every constraint, through `pg_get_constraintdef`; and the indexes that are
/// not already implied by one, through `pg_get_indexdef`.
///
/// **What it does not**: storage parameters, tablespaces, collations, partition
/// bounds and the `PARTITION BY` clause itself, inheritance, row-level
/// security, rules, generated and identity columns — which come out as plain
/// ones rather than as wrong ones, see `COLUMNS` — and anything an extension
/// added. A table using any of them
/// comes back as a statement that runs and produces something subtly
/// different — which is why the type is [`Ddl`] rather than a string, and why
/// a front-end showing it has to say it was generated.
fn ddl(schema: &str, name: &str, detail: &TableDetail, view_body: Option<&str>) -> Option<Ddl> {
    let qualified = format!("{}.{}", quoted(schema), quoted(name));

    // A view's body is the server's own text, and `CREATE VIEW` around it is
    // this client's — which is why the whole is still generated.
    if let Some(body) = view_body {
        let keyword = match detail.kind {
            RelationKind::MaterializedView => "MATERIALIZED VIEW",
            _ => "VIEW",
        };
        return Some(Ddl::generated(format!(
            "CREATE {keyword} {qualified} AS\n{}",
            body.trim_end()
        )));
    }
    // Only a table has columns to write out. A foreign table's options are not
    // something this can reconstruct, and a half-statement is worse than none.
    // The relations `relation_kind` cannot name — an index, a sequence — are
    // stopped by the caller, which still has the `relkind` letter.
    if detail.kind != RelationKind::Table || detail.columns.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = detail
        .columns
        .iter()
        .map(|column| {
            let mut line = format!("    {} {}", quoted(&column.name), column.type_name);
            if let Some(default) = &column.default {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            if !column.nullable {
                line.push_str(" NOT NULL");
            }
            line
        })
        .collect();
    // Constraints inline, in the server's own words. Named, because a
    // constraint whose name is dropped comes back with one PostgreSQL invents
    // — and the difference shows up the next time somebody drops it by name.
    lines.extend(
        rows_of(detail, "Constraints")
            .map(|row| format!("    CONSTRAINT {} {}", quoted(&row.0), row.2)),
    );

    let mut statement = format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"));

    // Indexes that no constraint already implies. A primary key and a unique
    // constraint each own an index of the same name, and writing both means a
    // statement that fails on the second.
    let owned: Vec<String> = rows_of(detail, "Constraints").map(|row| row.0).collect();
    for (index, _, definition) in rows_of(detail, "Indexes") {
        if !owned.contains(&index) {
            statement.push_str(&format!("\n\n{definition};"));
        }
    }
    Some(Ddl::generated(statement))
}

/// The `(name, kind, definition)` of each row of a section this driver built.
///
/// Reading them back out rather than keeping a second copy while building:
/// the sections are the answer, and a parallel structure to generate from
/// would be one more thing to keep in step with them.
fn rows_of<'a>(
    detail: &'a TableDetail,
    title: &str,
) -> impl Iterator<Item = (String, String, String)> + 'a {
    detail
        .section(title)
        .into_iter()
        .flat_map(|section| section.table.rows.iter())
        .filter_map(|row| {
            Some((
                text(row.get(0))?,
                text(row.get(1))?,
                // A definition can be null — see `definition` above — and a
                // statement built around a missing one would be nonsense.
                text(row.get(2))?,
            ))
        })
}

fn text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

/// An identifier, quoted the way PostgreSQL wants it.
///
/// Always, rather than only where it is needed: a name that does not need
/// quoting is unchanged by it except for the quotes, and deciding per name is
/// how a table called `order` ends up in a statement that will not parse.
fn quoted(name: &str) -> String {
    Ident::new(name)
        .quote(QuoteStyle::DoubleQuote)
        .as_str()
        .to_owned()
}

fn section(title: &str, columns: Vec<Column>, rows: Vec<Row>) -> Option<DetailSection> {
    (!rows.is_empty()).then(|| DetailSection {
        title: title.to_owned(),
        table: ResultSet::new(columns, rows, None),
    })
}

/// One `pg_get_*def` column, which can come back null.
///
/// The catalogue scan and the lookup these functions do inside themselves are
/// not one snapshot, so an index dropped by somebody else mid-query answers a
/// row whose definition is null. Read as `String` that is a panic in the
/// middle of a definition; a null cell is what it actually is.
fn definition(row: &tokio_postgres::Row, at: usize) -> Value {
    row.get::<_, Option<String>>(at)
        .map_or(Value::Null, Value::Text)
}

fn indexes_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Indexes",
        vec![
            Column::new("name", "text", false),
            Column::new("kind", "text", false),
            Column::new("definition", "text", true),
        ],
        rows.iter()
            .map(|row| {
                let (primary, unique): (bool, bool) = (row.get(2), row.get(3));
                Row(vec![
                    Value::Text(row.get(0)),
                    // Primary before unique: a primary key is unique too, and
                    // saying so is less useful than saying which one it is.
                    Value::Text(
                        if primary {
                            "primary key"
                        } else if unique {
                            "unique"
                        } else {
                            "index"
                        }
                        .to_owned(),
                    ),
                    definition(row, 1),
                ])
            })
            .collect(),
    )
}

fn triggers_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Triggers",
        vec![
            Column::new("name", "text", false),
            Column::new("definition", "text", true),
        ],
        rows.iter()
            .map(|row| Row(vec![Value::Text(row.get(0)), definition(row, 1)]))
            .collect(),
    )
}

fn constraints_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Constraints",
        vec![
            Column::new("name", "text", false),
            Column::new("kind", "text", false),
            Column::new("definition", "text", true),
        ],
        rows.iter()
            .map(|row| {
                Row(vec![
                    Value::Text(row.get(0)),
                    Value::Text(constraint_kind(row.get(1))),
                    definition(row, 2),
                ])
            })
            .collect(),
    )
}

/// A partitioning section, or none at all for a table that is not partitioned.
///
/// The query answers no rows for an unpartitioned table — `relkind = 'p'` —
/// and one row with a null child for a partitioned one with no partitions yet,
/// which is a real state and worth showing: the key is set and nothing has
/// been created under it.
fn partitioning_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Partitioning",
        vec![
            Column::new("key", "text", true),
            Column::new("partition", "text", true),
            Column::new("bounds", "text", true),
        ],
        rows.iter()
            .map(|row| {
                Row(vec![
                    definition(row, 0),
                    definition(row, 1),
                    definition(row, 2),
                ])
            })
            .collect(),
    )
}

/// `contype` as a word.
///
/// Anything unrecognised keeps its letter rather than being called something
/// it is not: a new constraint kind in a future PostgreSQL is better shown as
/// `x` than as `check`.
fn constraint_kind(contype: i8) -> String {
    match u8::try_from(contype).map(char::from) {
        Ok('p') => "primary key".to_owned(),
        Ok('f') => "foreign key".to_owned(),
        Ok('u') => "unique".to_owned(),
        Ok('c') => "check".to_owned(),
        Ok('t') => "constraint trigger".to_owned(),
        Ok('x') => "exclusion".to_owned(),
        // PostgreSQL 18 gives every `NOT NULL` a `pg_constraint` row. Shown as
        // a word like the rest rather than filtered out: it is a constraint
        // the server now names, and a bare `n` is the letter this function
        // exists to avoid.
        // Filtered out by `CONSTRAINTS`, and kept here anyway: a name is
        // cheaper than a letter appearing if one ever reaches this by another
        // route.
        Ok('n') => "not null".to_owned(),
        Ok(other) => other.to_string(),
        Err(_) => "unknown".to_owned(),
    }
}

/// The schema and relation names out of a path.
///
/// A path naming another database is refused rather than answered about this
/// one: PostgreSQL has no cross-database queries, so it needs another
/// connection, and dropping the segment instead would describe a same-named
/// relation here as though it were the one that was asked for. [`crate::preview`]
/// refuses it for the same reason.
fn split(database: &str, table: &TableRef) -> DriverResult<(String, String)> {
    match table.path.as_slice() {
        [db, schema, name] if db == database => Ok((schema.clone(), name.clone())),
        [db, _, _] => Err(DriverError::NotFound(format!(
            "{table} is in database `{db}`, and this connection is to `{database}`"
        ))),
        // Two segments are already about this connection, so there is nothing
        // to disagree with.
        [schema, name] => Ok((schema.clone(), name.clone())),
        _ => Err(DriverError::NotFound(format!(
            "`{table}` does not name a relation"
        ))),
    }
}

fn query(err: tokio_postgres::Error) -> DriverError {
    DriverError::query(crate::describe(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_with_or_without_its_database_names_the_same_relation() {
        // The tree gives three segments and an agent may give two, and both
        // mean the relation on this connection.
        let with = TableRef::new(["app", "public", "users"]);
        let without = TableRef::new(["public", "users"]);
        assert_eq!(
            split("app", &with).unwrap(),
            split("app", &without).unwrap()
        );
        assert_eq!(
            split("app", &with).unwrap(),
            ("public".to_owned(), "users".to_owned())
        );
    }

    #[test]
    fn a_path_naming_another_database_is_refused() {
        // Dropping the segment would answer about `app.public.users` under the
        // name `other.public.users`: the wrong relation, reported as the right
        // one. `preview` refuses the same path rather than paging it.
        let err = split("app", &TableRef::new(["other", "public", "users"])).unwrap_err();
        assert!(matches!(err, DriverError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn a_path_that_names_no_relation_is_refused_before_a_query() {
        for path in [vec!["public"], vec![], vec!["a", "b", "c", "d"]] {
            assert!(
                split("app", &TableRef::new(path.clone())).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_generated_column_has_no_default_to_write_out() {
        // `pg_get_expr` on a generated column returns the *generation*
        // expression, and `DEFAULT <that>` is a statement that runs and builds
        // a column which is neither generated nor an identity. Omitting is a
        // gap; emitting it is a lie.
        assert!(COLUMNS.contains("a.attgenerated = ''"));
        assert!(COLUMNS.contains("a.attidentity = ''"));
    }

    #[test]
    fn an_identifier_is_always_quoted() {
        // Deciding per name is how a table called `order` ends up in a
        // statement that will not parse, and a name that did not need it is
        // unchanged except for the quotes.
        assert_eq!(quoted("users"), "\"users\"");
        assert_eq!(quoted("order"), "\"order\"");
        // A quote inside a name is doubled, which is the standard escape.
        assert_eq!(quoted("od\"d"), "\"od\"\"d\"");
    }

    #[test]
    fn a_relation_that_is_not_one_of_the_four_gets_no_statement() {
        // `relation_kind` calls an index and a sequence tables, so without the
        // letter a `CREATE TABLE` would be built out of `pg_attribute`'s idea
        // of their columns.
        for letter in *b"iSct" {
            assert!(!has_ddl(letter as i8), "{}", letter as char);
        }
        for letter in *b"rpvm" {
            assert!(has_ddl(letter as i8), "{}", letter as char);
        }
    }

    #[test]
    fn only_a_table_gets_a_generated_create_table() {
        // A foreign table's options and an index's own relation are not
        // something this can reconstruct, and half a statement is worse than
        // none.
        let column = ColumnDef {
            name: "id".to_owned(),
            type_name: "integer".to_owned(),
            nullable: false,
            default: None,
            comment: None,
        };
        for kind in [RelationKind::External, RelationKind::Routine] {
            let detail =
                TableDetail::new(TableRef::new(["public", "t"]), kind, vec![column.clone()]);
            assert!(ddl("public", "t", &detail, None).is_none(), "{kind:?}");
        }

        let table = TableDetail::new(
            TableRef::new(["public", "t"]),
            RelationKind::Table,
            vec![column],
        );
        let statement = ddl("public", "t", &table, None).expect("a table has one");
        assert_eq!(
            statement.text(),
            "CREATE TABLE \"public\".\"t\" (\n    \"id\" integer NOT NULL\n);"
        );
    }

    #[test]
    fn a_view_keeps_the_body_the_server_gave() {
        let detail = TableDetail::new(
            TableRef::new(["public", "v"]),
            RelationKind::View,
            Vec::new(),
        );
        let statement = ddl("public", "v", &detail, Some("SELECT 1;\n")).expect("a view has one");
        assert_eq!(
            statement.text(),
            "CREATE VIEW \"public\".\"v\" AS\nSELECT 1;"
        );

        let mut materialized = detail;
        materialized.kind = RelationKind::MaterializedView;
        assert!(
            ddl("public", "v", &materialized, Some("SELECT 1"))
                .expect("one")
                .text()
                .starts_with("CREATE MATERIALIZED VIEW"),
        );
    }

    #[test]
    fn an_index_a_constraint_already_owns_is_not_written_twice() {
        // A primary key owns an index of the same name. Writing both means a
        // statement that fails on the second.
        let mut detail = TableDetail::new(
            TableRef::new(["public", "t"]),
            RelationKind::Table,
            vec![ColumnDef {
                name: "id".to_owned(),
                type_name: "integer".to_owned(),
                nullable: false,
                default: None,
                comment: None,
            }],
        );
        detail.sections.push(DetailSection {
            title: "Constraints".to_owned(),
            table: ResultSet::new(
                vec![
                    Column::new("name", "text", false),
                    Column::new("kind", "text", false),
                    Column::new("definition", "text", false),
                ],
                vec![Row(vec![
                    Value::Text("t_pkey".to_owned()),
                    Value::Text("primary key".to_owned()),
                    Value::Text("PRIMARY KEY (id)".to_owned()),
                ])],
                None,
            ),
        });
        detail.sections.push(DetailSection {
            title: "Indexes".to_owned(),
            table: ResultSet::new(
                vec![
                    Column::new("name", "text", false),
                    Column::new("kind", "text", false),
                    Column::new("definition", "text", false),
                ],
                vec![
                    Row(vec![
                        Value::Text("t_pkey".to_owned()),
                        Value::Text("primary key".to_owned()),
                        Value::Text("CREATE UNIQUE INDEX t_pkey ON t (id)".to_owned()),
                    ]),
                    Row(vec![
                        Value::Text("t_name".to_owned()),
                        Value::Text("index".to_owned()),
                        Value::Text("CREATE INDEX t_name ON t (name)".to_owned()),
                    ]),
                ],
                None,
            ),
        });

        let statement = ddl("public", "t", &detail, None)
            .expect("one")
            .text()
            .to_owned();
        assert!(
            statement.contains("CONSTRAINT \"t_pkey\" PRIMARY KEY (id)"),
            "{statement}"
        );
        assert!(statement.contains("CREATE INDEX t_name"), "{statement}");
        assert!(
            !statement.contains("CREATE UNIQUE INDEX t_pkey"),
            "{statement}"
        );
    }

    #[test]
    fn a_constraint_letter_becomes_a_word_or_stays_a_letter() {
        assert_eq!(constraint_kind(b'p' as i8), "primary key");
        assert_eq!(constraint_kind(b'f' as i8), "foreign key");
        assert_eq!(constraint_kind(b'c' as i8), "check");
        // PostgreSQL 18 catalogues `NOT NULL`, and a section full of `n` is
        // what leaving it out of the match looks like.
        assert_eq!(constraint_kind(b'n' as i8), "not null");
        // A kind a future PostgreSQL adds is better shown as its letter than
        // as something it is not.
        assert_eq!(constraint_kind(b'z' as i8), "z");
    }

    #[test]
    fn an_empty_section_is_no_section() {
        // An "Indexes" heading over nothing reads as both "this table has
        // none" and "the driver could not ask", and only one of those is worth
        // a heading.
        assert!(
            section(
                "Indexes",
                vec![Column::new("name", "text", false)],
                Vec::new()
            )
            .is_none()
        );
        assert!(
            section(
                "Indexes",
                vec![Column::new("name", "text", false)],
                vec![Row(vec![Value::Text("users_pkey".to_owned())])],
            )
            .is_some()
        );
    }

    #[test]
    fn the_trigger_query_leaves_out_the_ones_nobody_wrote() {
        // PostgreSQL creates three internal triggers per foreign key. Listing
        // them buries a user's own under bookkeeping they cannot edit.
        assert!(TRIGGERS.contains("NOT t.tgisinternal"));
    }

    #[test]
    fn the_constraint_query_leaves_out_per_column_not_nulls() {
        // PostgreSQL 18 catalogues every `NOT NULL` as a constraint of its
        // own. Without this a forty-column table answers with forty rows
        // repeating what the column list already says — and the conformance
        // container pins an older server, so nothing else would notice.
        assert!(CONSTRAINTS.contains("con.contype <> 'n'"));
    }

    #[test]
    fn the_partition_query_asks_only_about_partitioned_tables() {
        // Without `relkind = 'p'` the same join answers with an inheritance
        // child list, which is a different thing under the same heading.
        assert!(PARTITIONS.contains("c.relkind = 'p'"));
    }

    #[test]
    fn the_column_query_leaves_out_what_is_not_a_column() {
        // Both filters are load-bearing and neither is obvious: without the
        // first, every table has six system columns; without the second, a
        // dropped one shows up under a name PostgreSQL invented.
        assert!(COLUMNS.contains("a.attnum > 0"));
        assert!(COLUMNS.contains("NOT a.attisdropped"));
    }
}
