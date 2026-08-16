//! Turning `BQError` into something a person can act on.
//!
//! The crate's own `Display` for an API failure is `"Response error (error:
//! ResponseError { error: NestedResponseError { code: 403, errors: [...],
//! message: \"...\", status: \"...\" } })"` — a `Debug` dump of a struct, in a
//! status bar. Google's `message` field is already a sentence written for the
//! person who has to fix it, so that is what gets shown, and the HTTP code
//! goes with it because 401 and 403 mean different things to do.

use gcp_bigquery_client::error::BQError;
use sqlake_core::driver::DriverError;

/// A failure while opening a connection.
pub fn connect_failed(err: BQError) -> DriverError {
    DriverError::Connect(describe(err))
}

/// A failure with no better home. Not `Unsupported`: that means the caller
/// asked for something the driver does not do.
pub fn driver_error(what: impl Into<String>) -> DriverError {
    DriverError::Query(what.into())
}

/// What went wrong, as a sentence.
pub fn describe(err: BQError) -> String {
    match err {
        BQError::ResponseError { error } => {
            let e = error.error;
            format!("{} (HTTP {})", e.message, e.code)
        }
        // Every authentication variant is `Debug`-formatted by the crate too,
        // and unlike the API errors there is no better field inside them: what
        // is left is the crate's own text, which at least names the step.
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use gcp_bigquery_client::error::{NestedResponseError, ResponseError};

    use super::*;

    fn api_error(code: i64, message: &str) -> BQError {
        BQError::ResponseError {
            error: ResponseError {
                error: NestedResponseError {
                    code,
                    errors: Vec::new(),
                    message: message.to_owned(),
                    status: "PERMISSION_DENIED".to_owned(),
                },
            },
        }
    }

    #[test]
    fn an_api_failure_reads_as_google_wrote_it() {
        let message = "Access Denied: Project analytics-prod: \
                       User does not have bigquery.datasets.get permission.";
        let described = describe(api_error(403, message));
        assert!(described.starts_with(message), "{described}");
        assert!(described.contains("403"), "{described}");
        // The struct dump the crate's own `Display` would have produced.
        assert!(!described.contains("NestedResponseError"), "{described}");
    }

    #[test]
    fn a_refused_connection_is_a_connect_error() {
        // `is_retryable` reads this: a 403 is not worth retrying, but the
        // variant is what the store branches on, and `Connect` is the one
        // that puts the reason on the connection's own row.
        let err = connect_failed(api_error(401, "Invalid Credentials"));
        assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
        assert!(err.to_string().contains("Invalid Credentials"), "{err}");
    }
}
