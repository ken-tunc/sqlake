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
    pub issuer: Issuer,
}

/// Who asked for a statement to be run.
///
/// One history holds both, which is the point: when something unexpected has
/// happened to the data, there is one place to look and the answer to "was
/// that me?" is in it. Not a name — this client has no idea who is at the
/// keyboard — but which side of the socket the request came from, which is the
/// half it can actually know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Issuer {
    /// Somebody at this terminal.
    #[default]
    Human,
    /// Something on the socket: an agent, or a one-shot command.
    Agent,
}

impl Issuer {
    /// The word the file keeps, which outlives the build that wrote it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    /// And back. An unknown word is `None` rather than a guess: a later build
    /// may write one this does not know, and reading it as "human" would be
    /// the one answer that is never worth inventing.
    #[must_use]
    pub fn named(word: &str) -> Option<Self> {
        match word {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
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

    /// Recorded runs, newest first, matching what was asked for.
    fn search(&self, search: &Search) -> LibraryResult<Vec<HistoryEntry>>;
}

/// What to look for in the history.
///
/// `terms` is what somebody typed, not a query language: the search runs on
/// every keystroke, and a syntax somebody can get wrong halfway through typing
/// it is one that turns the pane red on the way to the answer. What the words
/// mean is the implementation's to decide — [`sqlake-library`] reads them as
/// "all of these, the last one as a prefix".
///
/// [`sqlake-library`]: https://github.com/ken-tunc/sqlake
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub terms: String,
    /// The most rows to answer with. There is no paging: a history is searched
    /// rather than read through, and a search nobody has narrowed enough to
    /// fit is one to narrow rather than to page.
    pub limit: usize,
}

impl Search {
    /// Everything, newest first.
    #[must_use]
    pub const fn newest(limit: usize) -> Self {
        Self {
            terms: String::new(),
            limit,
        }
    }
}

/// One run, as the file kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: RunId,
    pub connection: String,
    /// `None` for a row written before this build, or by a later one whose
    /// word for it this does not know.
    pub issuer: Option<Issuer>,
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
