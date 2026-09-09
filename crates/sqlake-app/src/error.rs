//! Failures at the application layer.

use sqlake_core::driver::DriverError;
use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Driver(#[from] DriverError),

    /// The file holding templates and history said no. Its own variant rather
    /// than `Refused`, because where to go looking is different again: not the
    /// server's log and not this client's rules, but a file on this disk.
    #[error(transparent)]
    Library(#[from] sqlake_core::library::LibraryError),

    /// The session actor is gone, so the connection is effectively closed.
    #[error("connection is closed")]
    SessionClosed,

    #[error("no driver registered for {0}")]
    UnknownDriver(&'static str),

    /// The profile could not be turned into something connectable: it does not
    /// exist, or its secret could not be read. The message comes from whatever
    /// implements `Profiles` and is already written for the person reading it.
    #[error("{0}")]
    Profile(String),

    #[error("no such connection")]
    UnknownConnection,

    /// Refused here, before anything was sent. Kept apart from
    /// [`AppError::Driver`] because the difference is where to go looking: a
    /// driver error is in the server's log, and this one is not — saying the
    /// server refused something it never saw sends somebody to read a log with
    /// nothing in it.
    #[error("{0}")]
    Refused(String),
}

impl AppError {
    /// Where in the statement, when the driver worked it out.
    ///
    /// Only a query failure has one, and only for the statement that was sent
    /// — which is why it travels on the error rather than being derived from
    /// the text afterwards.
    #[must_use]
    pub const fn at(&self) -> Option<sqlake_core::sql::Position> {
        match self {
            Self::Driver(DriverError::Query { at, .. }) => *at,
            _ => None,
        }
    }

    /// The single-line form a front-end shows next to whatever failed.
    #[must_use]
    pub fn user_message(&self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_failures_pass_their_own_message_through() {
        let err = AppError::from(DriverError::NotFound("public.users".into()));
        assert_eq!(err.user_message(), "not found: public.users");
    }

    #[test]
    fn a_dead_actor_reads_as_a_closed_connection() {
        assert_eq!(
            AppError::SessionClosed.user_message(),
            "connection is closed"
        );
    }
}
