//! What outlives a session: the statements somebody saved, and the ones they
//! have run.
//!
//! A trait here and an implementation in `sqlake-library`, for the reason
//! [`Profiles`](crate::profile::Profiles) is one: the application layer holds
//! a `dyn Library` rather than opening a file itself, so a test can hand it
//! something that keeps nothing and the suite still runs with no state
//! directory.
//!
//! Every method **blocks**. It is a file on a disk, and pretending otherwise
//! in the signature would move the lie rather than remove it — the caller runs
//! these on a blocking task, which is what the store already does with
//! `Profiles::resolve`.

use std::fmt;

use thiserror::Error;
use time::OffsetDateTime;

use crate::capability::DriverKind;
use crate::id::ConnId;

/// A template's identity, which the file assigns.
///
/// Not in [`id`](crate::id) with the others: those are made in this process
/// and are unique because a UUID is, while this one is a row that was written
/// once and is the same row on the next run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TemplateId(i64);

impl TemplateId {
    #[must_use]
    pub const fn new(row: i64) -> Self {
        Self(row)
    }

    #[must_use]
    pub const fn row(self) -> i64 {
        self.0
    }
}

impl fmt::Display for TemplateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One recorded run's identity. Assigned when the run starts, because the row
/// is written then — see [`Library::started`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(i64);

impl RunId {
    #[must_use]
    pub const fn new(row: i64) -> Self {
        Self(row)
    }

    #[must_use]
    pub const fn row(self) -> i64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A saved statement, with the placeholders still in it.
///
/// The body is text and stays text: what a person picked out of the palette
/// has to be readable in the buffer afterwards, and a template that was
/// compiled into something else could not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub id: TemplateId,
    pub name: String,
    pub body: String,
    /// `None` means any driver. A statement that is only valid on one of them
    /// says so, and one that is valid on both should not have to claim a side.
    pub driver: Option<DriverKind>,
    pub tags: Vec<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A template before the file has given it an id or its timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTemplate {
    pub name: String,
    pub body: String,
    pub driver: Option<DriverKind>,
    pub tags: Vec<String>,
}

/// What is known about a run when it starts.
///
/// `connection` is the id of the connection it went out on, kept as text: it
/// is this session's id and means nothing on the next run, but it is what
/// distinguishes two statements run against different databases within one
/// history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    pub connection: ConnId,
    pub driver: DriverKind,
    pub sql: String,
    pub started_at: OffsetDateTime,
}

/// How a run ended, and what it cost.
///
/// Cancelled is its own status rather than an error with a message: a
/// cancelled query is one somebody stopped on purpose, and a history that
/// files it under failures is a history that reports the user's own decisions
/// as things going wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Ok {
        duration_ms: u64,
        row_count: Option<u64>,
        bytes_processed: Option<u64>,
    },
    Failed {
        duration_ms: u64,
        message: String,
    },
    Cancelled {
        duration_ms: u64,
    },
    /// Costed, refused by the budget, and never sent.
    ///
    /// Its own status rather than an error: nothing went wrong and nothing
    /// ran. It is in the history at all because "the expensive one I decided
    /// not to run" is a thing somebody goes looking for — and because a
    /// statement that vanished from the history the moment it was refused
    /// would look like one that was never typed.
    Refused {
        duration_ms: u64,
        message: String,
    },
}

impl RunOutcome {
    /// The word this is filed under, which is also what the column holds.
    #[must_use]
    pub const fn status(&self) -> &'static str {
        match self {
            Self::Ok { .. } => "ok",
            Self::Failed { .. } => "error",
            Self::Cancelled { .. } => "cancelled",
            Self::Refused { .. } => "refused",
        }
    }

    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        match self {
            Self::Ok { duration_ms, .. }
            | Self::Failed { duration_ms, .. }
            | Self::Refused { duration_ms, .. }
            | Self::Cancelled { duration_ms } => *duration_ms,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LibraryError {
    /// A template with this name is already saved.
    ///
    /// Its own variant because it is the one failure here a person caused and
    /// can fix, and "UNIQUE constraint failed: templates.name" is not how to
    /// tell them.
    #[error("a template called `{name}` is already saved")]
    NameTaken { name: String },
    /// The row is not there — deleted in another window, or never written.
    #[error("no {what} with id {id}")]
    NoSuchRow { what: &'static str, id: i64 },
    /// The file said no. The message is SQLite's own, which is the only party
    /// that knows.
    #[error("{0}")]
    Failed(String),
}

pub type LibraryResult<T> = Result<T, LibraryError>;

/// Everything sqlake keeps between runs.
///
/// One trait rather than one per table: they are one file, and a caller
/// holding two handles to it would have two answers to "is this open".
pub trait Library: Send + Sync + fmt::Debug {
    /// Every saved template, newest first.
    fn templates(&self) -> LibraryResult<Vec<Template>>;

    fn add(&self, template: NewTemplate) -> LibraryResult<Template>;

    /// Overwrite everything about a template except when it was created.
    fn replace(&self, id: TemplateId, with: NewTemplate) -> LibraryResult<Template>;

    fn remove(&self, id: TemplateId) -> LibraryResult<()>;

    /// Record that a statement was sent, and answer with the row to settle.
    ///
    /// Written when the run starts rather than when it finishes so that the
    /// query somebody is waiting on — the one they are most likely to go
    /// looking for — is in the history while they wait.
    fn started(&self, run: RunStart) -> LibraryResult<RunId>;

    fn settled(&self, id: RunId, outcome: RunOutcome) -> LibraryResult<()>;

    /// Every recorded run, newest first.
    ///
    /// Here rather than waiting for M8's search because a history nothing can
    /// read is a history nothing can hold to be right: this is what says a run
    /// left the row it was supposed to.
    fn history(&self, limit: usize) -> LibraryResult<Vec<HistoryEntry>>;
}

/// One run, as the file kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: RunId,
    pub connection: String,
    pub driver: Option<DriverKind>,
    pub sql: String,
    pub started_at: OffsetDateTime,
    /// `None` while it is still running, which is the state a row is written
    /// in.
    pub status: Option<String>,
    pub duration_ms: Option<u64>,
    pub row_count: Option<u64>,
    pub bytes_processed: Option<u64>,
    pub error: Option<String>,
}
