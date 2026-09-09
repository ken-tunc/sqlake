//! The file sqlake keeps its templates and its history in.
//!
//! One SQLite file under the state directory, and the only implementation of
//! [`Library`]. It depends on `sqlake-core` and nothing else in the workspace,
//! the way a driver does: the application layer is handed one of these and
//! never learns that it is SQL underneath.
//!
//! Why SQLite and not a directory of TOML files, which is what
//! `connections.toml` is: history is searched, and searched incrementally
//! while somebody types. Templates alone would be happy as files — history is
//! what makes it a database, and two stores for one feature is one too many.

mod error;
mod schema;

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};
use time::OffsetDateTime;

use sqlake_core::capability::DriverKind;
use sqlake_core::library::{
    HistoryEntry, Library, LibraryError, LibraryResult, NewTemplate, RunId, RunOutcome, RunStart,
    Template, TemplateId,
};

use crate::error::{taken, translate};

/// The library, open.
///
/// A `Mutex` around one connection rather than a pool or a thread of its own.
/// The lock is held for the length of one statement against a local file, and
/// the writes here are a saved template and a row per query — this is not a
/// contended resource, and a pool would be a second thing to size for no gain.
/// It also gives the connection one owner, which is what makes "two writes at
/// once" a question that cannot be asked.
#[derive(Debug)]
pub struct Sqlite {
    connection: Mutex<Connection>,
}

impl Sqlite {
    /// Open the file, creating and migrating it if it is not there yet.
    pub fn open(path: &Path) -> LibraryResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|why| LibraryError::Failed(format!("{}: {why}", parent.display())))?;
            restrict(parent)?;
        }
        let connection = Connection::open(path).map_err(translate)?;
        // Owner-only, because everything typed into a SQL buffer ends up here
        // and some of it is `CREATE ROLE … PASSWORD`. Set after opening rather
        // than before: SQLite creates the file, so there is nothing to set the
        // mode of until it has. The `-wal` and `-shm` files SQLite makes take
        // their mode from this one.
        restrict(path)?;
        // WAL because a second sqlake — an attached session and a one-shot
        // command are two processes — should not have to wait behind a reader
        // to write a history row. `NORMAL` is WAL's own recommendation: the
        // durability it gives up is the last few writes on a power cut, and
        // what is lost is a query somebody can see the result of on screen.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(translate)?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(translate)?;
        // WAL lets a second process read while this one writes; it does not
        // let two of them write at once, and without a timeout the loser is
        // told "database is locked" straight away. An attached session and a
        // one-shot command sharing the file is the ordinary case here, so the
        // one that arrives second waits rather than failing — five seconds is
        // far longer than the writes this does and short enough that a wedged
        // lock is still reported rather than hung on.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(translate)?;
        Self::from(connection)
    }

    /// A library that is gone when the process is.
    ///
    /// What the tests use, and what a client whose state directory could not
    /// be opened falls back to: sqlake is a database client, and refusing to
    /// start because it cannot save a template would be the tail wagging the
    /// dog.
    pub fn in_memory() -> LibraryResult<Self> {
        Self::from(Connection::open_in_memory().map_err(translate)?)
    }

    fn from(mut connection: Connection) -> LibraryResult<Self> {
        // Off by default, and this file has a foreign key nowhere yet — turned
        // on so that the first one to be added is enforced rather than
        // decorative.
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(translate)?;
        schema::migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// What the file says its schema is. `0` is a file nothing has migrated.
    pub fn version(&self) -> LibraryResult<u32> {
        self.with(|connection| {
            connection
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .map_err(translate)
        })
    }

    /// The version this build migrates a file to.
    #[must_use]
    pub fn latest_version() -> u32 {
        schema::latest()
    }

    /// A poisoned lock means a panic while the file was being written, and the
    /// panic has already been reported by the hook that catches it. Taking the
    /// connection anyway is right for the same reason the panic hook restores
    /// the terminal rather than exiting: what is left is still openable, and
    /// SQLite's own transactions are what protect the file.
    fn with<T>(&self, f: impl FnOnce(&Connection) -> LibraryResult<T>) -> LibraryResult<T> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&connection)
    }
}

impl Library for Sqlite {
    fn templates(&self) -> LibraryResult<Vec<Template>> {
        self.with(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, name, body, driver, tags, created_at, updated_at \
                     FROM templates ORDER BY updated_at DESC, id DESC",
                )
                .map_err(translate)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(Template {
                        id: TemplateId::new(row.get(0)?),
                        name: row.get(1)?,
                        body: row.get(2)?,
                        driver: row.get::<_, Option<String>>(3)?.as_deref().and_then(driver),
                        tags: tags(&row.get::<_, String>(4)?),
                        created_at: moment(row.get(5)?),
                        updated_at: moment(row.get(6)?),
                    })
                })
                .map_err(translate)?;
            rows.collect::<Result<_, _>>().map_err(translate)
        })
    }

    fn add(&self, template: NewTemplate) -> LibraryResult<Template> {
        let now = now();
        self.with(|connection| {
            connection
                .execute(
                    "INSERT INTO templates (name, body, driver, tags, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![
                        template.name,
                        template.body,
                        template.driver.map(kind),
                        encode(&template.tags),
                        millis(now),
                    ],
                )
                .map_err(|why| taken(why, &template.name))?;
            Ok(Template {
                id: TemplateId::new(connection.last_insert_rowid()),
                name: template.name,
                body: template.body,
                driver: template.driver,
                tags: template.tags,
                created_at: now,
                updated_at: now,
            })
        })
    }

    fn replace(&self, id: TemplateId, with: NewTemplate) -> LibraryResult<Template> {
        let now = now();
        self.with(|connection| {
            let changed = connection
                .execute(
                    "UPDATE templates SET name = ?2, body = ?3, driver = ?4, tags = ?5, \
                     updated_at = ?6 WHERE id = ?1",
                    params![
                        id.row(),
                        with.name,
                        with.body,
                        with.driver.map(kind),
                        encode(&with.tags),
                        millis(now),
                    ],
                )
                .map_err(|why| taken(why, &with.name))?;
            if changed == 0 {
                return Err(LibraryError::NoSuchRow {
                    what: "template",
                    id: id.row(),
                });
            }
            // Read back rather than assembled from the arguments: `created_at`
            // is the one field this does not set, and inventing it here would
            // be a lie about when somebody wrote the template.
            let created: i64 = connection
                .query_row(
                    "SELECT created_at FROM templates WHERE id = ?1",
                    params![id.row()],
                    |row| row.get(0),
                )
                .map_err(translate)?;
            Ok(Template {
                id,
                name: with.name,
                body: with.body,
                driver: with.driver,
                tags: with.tags,
                created_at: moment(created),
                updated_at: now,
            })
        })
    }

    fn remove(&self, id: TemplateId) -> LibraryResult<()> {
        self.with(|connection| {
            let removed = connection
                .execute("DELETE FROM templates WHERE id = ?1", params![id.row()])
                .map_err(translate)?;
            if removed == 0 {
                return Err(LibraryError::NoSuchRow {
                    what: "template",
                    id: id.row(),
                });
            }
            Ok(())
        })
    }

    fn started(&self, run: RunStart) -> LibraryResult<RunId> {
        self.with(|connection| {
            connection
                .execute(
                    "INSERT INTO query_history (connection_id, driver, sql, started_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        run.connection.to_string(),
                        kind(run.driver),
                        run.sql,
                        millis(run.started_at),
                    ],
                )
                .map_err(translate)?;
            Ok(RunId::new(connection.last_insert_rowid()))
        })
    }

    fn history(&self, limit: usize) -> LibraryResult<Vec<HistoryEntry>> {
        self.with(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, connection_id, driver, sql, started_at, status, duration_ms, \
                     row_count, bytes_processed, error FROM query_history \
                     ORDER BY started_at DESC, id DESC LIMIT ?1",
                )
                .map_err(translate)?;
            let rows = statement
                .query_map([count(limit as u64)], |row| {
                    Ok(HistoryEntry {
                        id: RunId::new(row.get(0)?),
                        connection: row.get(1)?,
                        driver: driver(&row.get::<_, String>(2)?),
                        sql: row.get(3)?,
                        started_at: moment(row.get(4)?),
                        status: row.get(5)?,
                        duration_ms: row.get::<_, Option<i64>>(6)?.map(unsign),
                        row_count: row.get::<_, Option<i64>>(7)?.map(unsign),
                        bytes_processed: row.get::<_, Option<i64>>(8)?.map(unsign),
                        error: row.get(9)?,
                    })
                })
                .map_err(translate)?;
            rows.collect::<Result<_, _>>().map_err(translate)
        })
    }

    fn settled(&self, id: RunId, outcome: RunOutcome) -> LibraryResult<()> {
        // Widened to `i64` because that is the only integer SQLite has, and
        // a `u64` this client would have to have counted more rows than the
        // column can hold to reach.
        let (rows, bytes, message) = match &outcome {
            RunOutcome::Ok {
                row_count,
                bytes_processed,
                ..
            } => (row_count.map(count), bytes_processed.map(count), None),
            RunOutcome::Failed { message, .. } | RunOutcome::Refused { message, .. } => {
                (None, None, Some(message.clone()))
            }
            RunOutcome::Cancelled { .. } => (None, None, None),
        };
        self.with(|connection| {
            let changed = connection
                .execute(
                    "UPDATE query_history SET duration_ms = ?2, row_count = ?3, \
                     bytes_processed = ?4, status = ?5, error = ?6 WHERE id = ?1",
                    params![
                        id.row(),
                        count(outcome.duration_ms()),
                        rows,
                        bytes,
                        outcome.status(),
                        message,
                    ],
                )
                .map_err(translate)?;
            if changed == 0 {
                return Err(LibraryError::NoSuchRow {
                    what: "run",
                    id: id.row(),
                });
            }
            Ok(())
        })
    }
}

/// Owner-only, on the file and on the directory holding it.
///
/// Not on Windows, which has no mode to set: the equivalent there is an ACL,
/// and writing one badly would be worse than the default a home directory
/// already carries.
#[cfg(unix)]
fn restrict(path: &Path) -> LibraryResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|why| LibraryError::Failed(format!("{}: {why}", path.display())))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> LibraryResult<()> {
    Ok(())
}

/// Back from the only integer SQLite has. A negative one is a file somebody
/// edited by hand, and zero is a better answer than a number near `u64::MAX`.
fn unsign(of: i64) -> u64 {
    u64::try_from(of).unwrap_or(0)
}

/// Now, at the precision the file keeps.
///
/// Truncated here rather than on the way in, so that what a caller is handed
/// back is what the next read of the same row will say. A template whose
/// `updated_at` changes by a few microseconds the first time it is re-read is
/// a template that compares unequal to itself.
fn now() -> OffsetDateTime {
    moment(millis(OffsetDateTime::now_utc()))
}

/// A count as the only integer SQLite has, saturating rather than wrapping:
/// a negative row count read back later would be worse than a large one.
fn count(of: u64) -> i64 {
    i64::try_from(of).unwrap_or(i64::MAX)
}

/// Milliseconds since the epoch, UTC.
///
/// An integer rather than a string: it is what the history is ordered by, and
/// a text timestamp orders correctly only for as long as everybody writing it
/// agrees about the format. Local time is a rendering decision and is made
/// where every other one is.
fn millis(at: OffsetDateTime) -> i64 {
    (at.unix_timestamp_nanos() / 1_000_000)
        .try_into()
        .unwrap_or(i64::MAX)
}

fn moment(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The driver as the file spells it.
///
/// Its own word rather than `Debug`: this is written into a file that outlives
/// the build, and renaming a variant would then rename what is already saved.
const fn kind(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::Postgres => "postgres",
        DriverKind::BigQuery => "bigquery",
        DriverKind::Mock => "mock",
    }
}

/// And back — `None` for a word this build does not know, which is a template
/// saved by a later one. Dropping the restriction shows the template
/// everywhere rather than hiding it, and a template nobody can see is
/// indistinguishable from one that was lost.
fn driver(word: &str) -> Option<DriverKind> {
    match word {
        "postgres" => Some(DriverKind::Postgres),
        "bigquery" => Some(DriverKind::BigQuery),
        "mock" => Some(DriverKind::Mock),
        _ => None,
    }
}

/// Tags as a JSON array, because a tag with a comma in it is a thing somebody
/// will write and a comma-separated column cannot hold.
fn encode(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_owned())
}

fn tags(encoded: &str) -> Vec<String> {
    serde_json::from_str(encoded).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use sqlake_core::id::ConnId;

    use super::*;

    fn library() -> Sqlite {
        Sqlite::in_memory().expect("an in-memory library opens")
    }

    fn template(name: &str) -> NewTemplate {
        NewTemplate {
            name: name.to_owned(),
            body: "select * from {{table}}".to_owned(),
            driver: Some(DriverKind::Postgres),
            tags: vec!["daily".to_owned()],
        }
    }

    fn run() -> RunStart {
        RunStart {
            connection: ConnId::new(),
            driver: DriverKind::Mock,
            sql: "select 1".to_owned(),
            started_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn a_new_file_is_at_the_version_this_build_writes() {
        let library = library();
        assert_eq!(
            library.version().expect("a version"),
            Sqlite::latest_version()
        );
        assert!(Sqlite::latest_version() >= 1);
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        // Every open runs the migrations, so this is what every run after the
        // first does.
        let file = tempfile::tempdir().expect("a directory");
        let path = file.path().join("library.db");

        let first = Sqlite::open(&path).expect("it opens");
        first.add(template("kept")).expect("it saves");
        drop(first);

        let again = Sqlite::open(&path).expect("it opens again");
        assert_eq!(
            again.version().expect("a version"),
            Sqlite::latest_version()
        );
        assert_eq!(
            again.templates().expect("they are there").len(),
            1,
            "a second open should not have rebuilt the file"
        );
    }

    #[test]
    fn a_file_from_a_later_build_is_left_alone() {
        // Refusing it would mean somebody who ran a newer sqlake once cannot
        // open their own templates with this one.
        let file = tempfile::tempdir().expect("a directory");
        let path = file.path().join("library.db");
        let library = Sqlite::open(&path).expect("it opens");
        library
            .with(|connection| {
                connection
                    .pragma_update(None, "user_version", 999u32)
                    .map_err(translate)
            })
            .expect("the version is set");
        drop(library);

        let again = Sqlite::open(&path).expect("a newer file still opens");
        assert_eq!(again.version().expect("a version"), 999);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_the_owners_alone() {
        // Everything typed into a SQL buffer ends up in the history, and some
        // of it is a password. This is the whole of what protects it.
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a directory");
        let home = dir.path().join("state");
        let path = home.join("library.db");
        let library = Sqlite::open(&path).expect("it opens");
        library.add(template("secret")).expect("it saves");

        let mode = |at: &Path| {
            std::fs::metadata(at)
                .expect("it is there")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&home), 0o700, "the directory it sits in, too");
    }

    #[test]
    fn two_processes_can_share_the_file() {
        // An attached session and a one-shot command are two processes on one
        // library, which is what WAL and the busy timeout are for.
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("library.db");
        let one = Sqlite::open(&path).expect("the first opens");
        let two = Sqlite::open(&path).expect("the second opens");

        one.add(template("from one")).expect("the first writes");
        two.add(template("from two")).expect("the second writes");
        assert_eq!(one.templates().expect("listed").len(), 2);
        assert_eq!(two.templates().expect("listed").len(), 2);
    }

    #[test]
    fn a_file_on_disk_is_in_wal_mode() {
        // `pragma_update` is `execute` underneath, and `journal_mode` answers
        // with a row — so it is worth checking that the mode was set rather
        // than that the call did not fail.
        let dir = tempfile::tempdir().expect("a directory");
        let library = Sqlite::open(&dir.path().join("library.db")).expect("it opens");
        let mode: String = library
            .with(|connection| {
                connection
                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(translate)
            })
            .expect("it answers");
        assert_eq!(mode, "wal");
    }

    #[test]
    fn a_template_comes_back_the_way_it_went_in() {
        let library = library();
        let saved = library.add(template("daily rollup")).expect("it saves");
        assert_eq!(saved.name, "daily rollup");

        let held = library.templates().expect("they are listed");
        assert_eq!(held, vec![saved.clone()]);
        // Including the parts that go through a conversion on the way.
        assert_eq!(held[0].driver, Some(DriverKind::Postgres));
        assert_eq!(held[0].tags, ["daily"]);
    }

    #[test]
    fn a_tag_with_a_comma_in_it_survives() {
        // The reason tags are JSON and not a comma-separated column.
        let library = library();
        let saved = library
            .add(NewTemplate {
                tags: vec!["a,b".to_owned(), "c".to_owned()],
                ..template("commas")
            })
            .expect("it saves");
        assert_eq!(saved.tags, ["a,b", "c"]);
        assert_eq!(library.templates().expect("listed")[0].tags, ["a,b", "c"]);
    }

    #[test]
    fn two_templates_cannot_share_a_name() {
        let library = library();
        library.add(template("same")).expect("the first saves");
        let refused = library
            .add(template("same"))
            .expect_err("the second should not");
        assert_eq!(
            refused,
            LibraryError::NameTaken {
                name: "same".to_owned()
            }
        );
    }

    #[test]
    fn replacing_keeps_when_it_was_written() {
        let library = library();
        let saved = library.add(template("first name")).expect("it saves");
        let changed = library
            .replace(
                saved.id,
                NewTemplate {
                    name: "second name".to_owned(),
                    body: "select 2".to_owned(),
                    driver: None,
                    tags: Vec::new(),
                },
            )
            .expect("it replaces");

        assert_eq!(changed.id, saved.id);
        assert_eq!(changed.name, "second name");
        assert_eq!(changed.driver, None);
        assert_eq!(
            changed.created_at, saved.created_at,
            "editing a template does not make it new"
        );
        assert_eq!(library.templates().expect("listed"), vec![changed]);
    }

    #[test]
    fn a_template_that_is_not_there_is_said_to_be_missing() {
        // Rather than reporting success for having done nothing: the caller
        // asked about a row, and "there is no such row" is the answer.
        let library = library();
        let gone = TemplateId::new(404);
        assert!(matches!(
            library.remove(gone),
            Err(LibraryError::NoSuchRow {
                what: "template",
                ..
            })
        ));
        assert!(matches!(
            library.replace(gone, template("anything")),
            Err(LibraryError::NoSuchRow {
                what: "template",
                ..
            })
        ));
    }

    #[test]
    fn removing_takes_it_out_of_the_list() {
        let library = library();
        let saved = library.add(template("temporary")).expect("it saves");
        library.remove(saved.id).expect("it goes");
        assert!(library.templates().expect("listed").is_empty());
    }

    #[test]
    fn a_run_is_in_the_history_before_it_has_finished() {
        // The whole reason the row is written at the start: the query somebody
        // is waiting on is the one they are most likely to go looking for.
        let library = library();
        let id = library.started(run()).expect("it records");
        let (status, duration): (Option<String>, Option<i64>) = library
            .with(|connection| {
                connection
                    .query_row(
                        "SELECT status, duration_ms FROM query_history WHERE id = ?1",
                        params![id.row()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(translate)
            })
            .expect("the row is there");
        assert_eq!(status, None, "a running query has not ended yet");
        assert_eq!(duration, None);
    }

    #[test]
    fn how_a_run_ended_is_what_the_row_says() {
        let library = library();
        for (outcome, expected) in [
            (
                RunOutcome::Ok {
                    duration_ms: 12,
                    row_count: Some(3),
                    bytes_processed: Some(4096),
                },
                "ok",
            ),
            (
                RunOutcome::Failed {
                    duration_ms: 5,
                    message: "syntax error".to_owned(),
                },
                "error",
            ),
            (RunOutcome::Cancelled { duration_ms: 7 }, "cancelled"),
            (
                RunOutcome::Refused {
                    duration_ms: 2,
                    message: "over the budget".to_owned(),
                },
                "refused",
            ),
        ] {
            let id = library.started(run()).expect("it records");
            library.settled(id, outcome).expect("it settles");
            let status: String = library
                .with(|connection| {
                    connection
                        .query_row(
                            "SELECT status FROM query_history WHERE id = ?1",
                            params![id.row()],
                            |row| row.get(0),
                        )
                        .map_err(translate)
                })
                .expect("the row is there");
            assert_eq!(status, expected);
        }
    }

    #[test]
    fn the_history_reads_back_what_was_written() {
        let library = library();
        let id = library.started(run()).expect("it records");
        library
            .settled(
                id,
                RunOutcome::Ok {
                    duration_ms: 42,
                    row_count: Some(7),
                    bytes_processed: None,
                },
            )
            .expect("it settles");

        let held = library.history(10).expect("it reads");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id, id);
        assert_eq!(held[0].sql, "select 1");
        assert_eq!(held[0].driver, Some(DriverKind::Mock));
        assert_eq!(held[0].status.as_deref(), Some("ok"));
        assert_eq!(held[0].duration_ms, Some(42));
        assert_eq!(held[0].row_count, Some(7));
        assert_eq!(held[0].bytes_processed, None);
    }

    #[test]
    fn a_running_query_is_in_the_history_with_no_end_on_it() {
        let library = library();
        library.started(run()).expect("it records");
        let held = library.history(10).expect("it reads");
        assert_eq!(held[0].status, None);
        assert_eq!(held[0].duration_ms, None);
    }

    #[test]
    fn the_newest_run_is_first() {
        let library = library();
        for sql in ["first", "second"] {
            library
                .started(RunStart {
                    sql: sql.to_owned(),
                    ..run()
                })
                .expect("it records");
        }
        let held = library.history(10).expect("it reads");
        // Same millisecond, so the id is what breaks the tie — and it has to,
        // or two runs a second apart from each other read back in either
        // order.
        assert_eq!(held[0].sql, "second");
        assert_eq!(held[1].sql, "first");
    }

    #[test]
    fn the_search_index_follows_the_rows_it_indexes() {
        // An external-content FTS table indexes rows it does not own, so the
        // triggers are the only thing keeping the two in step. M8 searches
        // this; if it is wrong, M8 finds statements that are not there.
        let library = library();
        let id = library
            .started(RunStart {
                sql: "select price from orders".to_owned(),
                ..run()
            })
            .expect("it records");
        let found = |term: &str| -> i64 {
            library
                .with(|connection| {
                    connection
                        .query_row(
                            "SELECT count(*) FROM query_history_fts WHERE query_history_fts \
                             MATCH ?1",
                            params![term],
                            |row| row.get(0),
                        )
                        .map_err(translate)
                })
                .expect("the index answers")
        };
        assert_eq!(found("orders"), 1);

        library
            .with(|connection| {
                connection
                    .execute("DELETE FROM query_history WHERE id = ?1", params![id.row()])
                    .map_err(translate)
            })
            .expect("it is deleted");
        assert_eq!(found("orders"), 0, "the index kept a row that is gone");
    }

    #[test]
    fn a_driver_this_build_does_not_know_hides_nothing() {
        // A template saved by a later sqlake against a driver this one has
        // never heard of. Showing it everywhere is better than hiding it: a
        // template nobody can see is one that looks lost.
        let library = library();
        library.add(template("from the future")).expect("it saves");
        library
            .with(|connection| {
                connection
                    .execute("UPDATE templates SET driver = 'duckdb'", [])
                    .map_err(translate)
            })
            .expect("it is changed");
        assert_eq!(library.templates().expect("listed")[0].driver, None);
    }
}
