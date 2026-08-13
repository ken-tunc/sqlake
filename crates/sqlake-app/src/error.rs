//! Failures at the application layer.

use sqlake_core::driver::DriverError;
use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Driver(#[from] DriverError),

    /// The session actor is gone, so the connection is effectively closed.
    #[error("connection is closed")]
    SessionClosed,

    #[error("no driver registered for {0}")]
    UnknownDriver(&'static str),

    #[error("no such connection")]
    UnknownConnection,

    #[error("no such tab")]
    UnknownTab,
}

impl AppError {
    /// The single-line form shown in a toast or on a failed node.
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
