//! SQLite's failures in the words the rest of the client uses.

use rusqlite::Error as SqliteError;
use rusqlite::ErrorCode;
use rusqlite::ffi;

use sqlake_core::library::LibraryError;

/// Anything the file said no to.
///
/// The message is SQLite's own. Rewriting it would mean guessing at what went
/// wrong from a code that already carries the answer, and the one failure
/// worth naming here is caught before this is reached — see [`taken`].
pub(crate) fn translate(error: SqliteError) -> LibraryError {
    LibraryError::Failed(error.to_string())
}

/// The same, except that a clash on `templates.name` becomes the failure a
/// person can act on.
///
/// Checked here rather than with a `SELECT` before the `INSERT`: two windows
/// on one file can both find the name free and then both write it, and the
/// constraint is the only thing that sees the second one.
pub(crate) fn taken(error: SqliteError, name: &str) -> LibraryError {
    if let SqliteError::SqliteFailure(ffi::Error { code, .. }, _) = &error
        && *code == ErrorCode::ConstraintViolation
        // Any other constraint on this table is a bug in the statement rather
        // than something the person typed, and reporting it as a name clash
        // would send them to fix the wrong thing.
        && error.to_string().contains("templates.name")
    {
        return LibraryError::NameTaken {
            name: name.to_owned(),
        };
    }
    translate(error)
}
