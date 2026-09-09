//! The file's shape, and the way it moves forward.
//!
//! Migrations are a list of statements with an index, applied under
//! `user_version`, which is a four-byte integer SQLite keeps in the header for
//! exactly this. No migration table, no checksums: this file belongs to one
//! person on one machine, and the failure a heavier scheme protects against —
//! two deployments disagreeing about what has run — cannot happen here.
//!
//! A migration is never edited once it has shipped. Changing one changes the
//! schema of every file that already applied it, silently, and the version
//! number says the change was made.

use rusqlite::Connection;

use crate::error::translate;
use sqlake_core::library::LibraryResult;

/// Every migration, in order. The index in this list is the `user_version`
/// after it has been applied.
const MIGRATIONS: &[&str] = &[
    // v1 — templates and history, together, because M7 and M8 share the file
    // and cutting it twice is how the second one ends up bolted on.
    "
    CREATE TABLE templates (
        id         INTEGER PRIMARY KEY,
        name       TEXT NOT NULL UNIQUE,
        body       TEXT NOT NULL,
        driver     TEXT,
        tags       TEXT NOT NULL DEFAULT '[]',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE query_history (
        id              INTEGER PRIMARY KEY,
        connection_id   TEXT NOT NULL,
        driver          TEXT NOT NULL,
        sql             TEXT NOT NULL,
        started_at      INTEGER NOT NULL,
        duration_ms     INTEGER,
        row_count       INTEGER,
        bytes_processed INTEGER,
        status          TEXT,
        error           TEXT,
        pinned          INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX query_history_started_at ON query_history (started_at DESC);

    CREATE VIRTUAL TABLE query_history_fts USING fts5(
        sql,
        content='query_history',
        content_rowid='id'
    );

    -- An external-content FTS table holds no copy of the text; it holds an
    -- index into rows it does not own. Without these it goes on answering for
    -- rows that have been deleted and never sees rows that have been added,
    -- which is why they are part of the schema rather than something the
    -- milestone that writes the search remembers to add.
    CREATE TRIGGER query_history_ai AFTER INSERT ON query_history BEGIN
        INSERT INTO query_history_fts (rowid, sql) VALUES (new.id, new.sql);
    END;
    CREATE TRIGGER query_history_ad AFTER DELETE ON query_history BEGIN
        INSERT INTO query_history_fts (query_history_fts, rowid, sql)
        VALUES ('delete', old.id, old.sql);
    END;
    CREATE TRIGGER query_history_au AFTER UPDATE ON query_history BEGIN
        INSERT INTO query_history_fts (query_history_fts, rowid, sql)
        VALUES ('delete', old.id, old.sql);
        INSERT INTO query_history_fts (rowid, sql) VALUES (new.id, new.sql);
    END;
    ",
];

/// What a file this build has finished with says it is.
#[must_use]
pub(crate) fn latest() -> u32 {
    u32::try_from(MIGRATIONS.len()).expect("a migration list that fits in a u32")
}

/// Bring a file up to [`latest`], or leave a newer one alone.
///
/// A file from a later build is opened read-write and not migrated backwards:
/// the alternative is to refuse it, and refusing means a person who ran a
/// newer sqlake once cannot open their own templates with this one. Nothing
/// here drops a column, so the older build reads what it understands and
/// ignores the rest.
pub(crate) fn migrate(connection: &mut Connection) -> LibraryResult<()> {
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(translate)?;

    for (at, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let applied = u32::try_from(at).expect("a migration index that fits in a u32") + 1;
        // One transaction for the statements and the number that says they
        // ran: a crash between the two would leave a file whose schema and
        // version disagree, which every later run would then act on.
        let step = connection.transaction().map_err(translate)?;
        step.execute_batch(migration).map_err(translate)?;
        step.pragma_update(None, "user_version", applied)
            .map_err(translate)?;
        step.commit().map_err(translate)?;
    }
    Ok(())
}
