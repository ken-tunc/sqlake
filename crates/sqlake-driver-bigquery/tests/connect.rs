//! Connecting, against a stub standing in for Google.
//!
//! Every call this driver makes is HTTPS to a service that bills for some of
//! them, so there is no version of this that talks to the real API. What the
//! stub can still prove is the whole of what `connect` promises: that a token
//! is fetched and used, that the project is checked before the connection is
//! reported as ready, and that a refusal arrives with Google's own words on it.
//!
//! Both auth modes end in the same two calls; what differs is which builder
//! method acquires the token. The service-account path is the one a test can
//! reach — ADC discovers credentials from the environment, and setting an
//! environment variable is `unsafe` in this edition and forbidden in this
//! workspace.

use std::time::Duration;

use sqlake_core::driver::{Driver, DriverError};
use sqlake_core::id::ProfileId;
use sqlake_core::profile::{Params, ResolvedProfile};
use wiremock::ResponseTemplate;

mod google;
use google::{Google, PROJECT, profile};

#[tokio::test]
async fn a_profile_connects_and_the_project_is_checked_on_the_way() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#datasetList",
            "etag": "an-etag",
            "datasets": [{
                "kind": "bigquery#dataset",
                "id": "analytics-prod:events",
                "datasetReference": { "projectId": PROJECT, "datasetId": "events" },
            }],
        })))
        .await;

    let session = google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect("should connect");

    assert!(session.capabilities().free_preview);
    // The check is not a formality: without a request to `datasets`, a project
    // nobody has access to would connect and fail on the first click instead.
    assert_eq!(google.requests_to_datasets().await, 1);
    session.close().await;
}

#[tokio::test]
async fn a_project_with_no_datasets_in_it_still_connects() {
    // The API omits `datasets` entirely when there are none, and
    // `gcp-bigquery-client` 0.28 models it as a plain `Vec`, so this perfectly
    // ordinary response is a decode error. A new project is exactly when
    // somebody first points this client at one, and refusing it would make
    // that the one project that cannot be opened.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#datasetList",
            "etag": "an-etag",
        })))
        .await;

    let session = google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect("an empty project is still a project");
    session.close().await;
}

#[tokio::test]
async fn the_wrong_credentials_fail_with_the_reason() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {
                "code": 403,
                "message": "Access Denied: Project analytics-prod: \
                            User does not have bigquery.datasets.get permission.",
                "errors": [],
                "status": "PERMISSION_DENIED",
            }
        })))
        .await;

    let err = google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect_err("should be refused");

    assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
    // What the user has to read to know what to do about it: the permission
    // that is missing, not a struct dump and not "response error".
    let message = err.to_string();
    assert!(message.contains("bigquery.datasets.get"), "{message}");
    assert!(message.contains("403"), "{message}");
}

#[tokio::test]
async fn a_key_the_token_endpoint_refuses_does_not_connect() {
    // The failure that does not look like one. A key is only read while the
    // client is built; it is first *used* by the call that verifies the
    // project, so a revoked key and an expired ADC login both arrive here as
    // an authentication error rather than as an HTTP refusal from BigQuery.
    let google = Google::start().await;
    google.refuses_a_token().await;

    let err = google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect_err("a credential that cannot get a token is not a connection");
    assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
}

#[tokio::test]
async fn a_key_file_that_is_not_there_says_so() {
    let google = Google::start().await;
    let err = google
        .driver()
        .connect(&profile("/no/such/key.json".into()))
        .await
        .expect_err("should be refused");

    assert!(matches!(err, DriverError::Connect(_)), "{err:?}");
    assert_eq!(google.requests_to_datasets().await, 0);
}

#[tokio::test]
async fn a_postgres_profile_is_refused_rather_than_misread() {
    let google = Google::start().await;
    let profile = ResolvedProfile {
        id: ProfileId::parse("prod-pg").unwrap(),
        readonly: false,
        params: Params::Mock,
    };
    let err = google
        .driver()
        .connect(&profile)
        .await
        .expect_err("should be refused");
    assert!(err.to_string().contains("not a bigquery profile"), "{err}");
}

#[tokio::test]
async fn a_server_that_never_answers_gives_up() {
    let google = Google::start().await;
    google.issues_a_token().await;
    // Accepts the request and says nothing — a proxy with no backend, which
    // has no timeout of its own anywhere in the stack below.
    google
        .answers_datasets(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .await;

    let err = google
        .driver()
        .with_deadline(Duration::from_millis(200))
        .connect(&profile(google.key_file()))
        .await
        .expect_err("should give up");

    assert!(err.to_string().contains("no answer"), "{err}");
    assert!(err.is_retryable(), "a hang is worth trying again");
}
