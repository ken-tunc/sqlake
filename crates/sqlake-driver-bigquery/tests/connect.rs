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
use sqlake_core::profile::{BigQueryAuth, BigQueryParams, Params, ResolvedProfile};
use sqlake_driver_bigquery::BqDriver;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT: &str = "analytics-prod";

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
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Invalid grant: account not found",
        })))
        .mount(&google.server)
        .await;

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

// ── the stub ───────────────────────────────────────────────────────────────

/// A mock server, and a service-account key file pointing its token endpoint
/// back at it.
struct Google {
    server: MockServer,
    // Held, not read: dropping it deletes the key file.
    _dir: tempfile::TempDir,
    key_file: std::path::PathBuf,
}

impl Google {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let key_file = dir.path().join("key.json");
        std::fs::write(&key_file, key(&server.uri())).expect("should write the key");
        Self {
            server,
            _dir: dir,
            key_file,
        }
    }

    fn driver(&self) -> BqDriver {
        BqDriver::new().with_api_url(self.server.uri())
    }

    fn key_file(&self) -> std::path::PathBuf {
        self.key_file.clone()
    }

    async fn issues_a_token(&self) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "a-token",
                "token_type": "bearer",
                "expires_in": 3600,
            })))
            .mount(&self.server)
            .await;
    }

    async fn answers_datasets(&self, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/projects/{PROJECT}/datasets")))
            // The token is not decoration: a request without it would be
            // answered by Google with a 401 and by a stub that ignores it with
            // whatever was mounted.
            .and(header("authorization", "Bearer a-token"))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    async fn requests_to_datasets(&self) -> usize {
        self.server
            .received_requests()
            .await
            .expect("the stub records requests")
            .iter()
            .filter(|r| r.url.path().ends_with("/datasets"))
            .count()
    }
}

fn profile(key_file: std::path::PathBuf) -> ResolvedProfile {
    ResolvedProfile {
        id: ProfileId::parse("bq").unwrap(),
        readonly: false,
        params: Params::BigQuery(BigQueryParams {
            project: PROJECT.to_owned(),
            location: None,
            auth: BigQueryAuth::ServiceAccount(key_file),
            max_bytes_billed: None,
        }),
    }
}

/// A syntactically valid service-account key whose `token_uri` is the stub.
///
/// The private key is a throwaway generated for this file: `yup-oauth2` signs
/// the token request with it, so a placeholder string would fail before any
/// request was made and the test would pass for the wrong reason.
fn key(server: &str) -> String {
    serde_json::json!({
        "type": "service_account",
        "project_id": PROJECT,
        "private_key_id": "0",
        "private_key": PRIVATE_KEY,
        "client_email": "sqlake@example.iam.gserviceaccount.com",
        "client_id": "0",
        "auth_uri": format!("{server}/auth"),
        "token_uri": format!("{server}/token"),
        "auth_provider_x509_cert_url": format!("{server}/certs"),
        "client_x509_cert_url": format!("{server}/robot"),
    })
    .to_string()
}

const PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCA4HpM6wFwgOI6\nrzNbWCY4VCOkRJvJEoF0OqMjf6GvQYHFLBr/p977dao76BySGrUrwLPMkdOrVGuz\nHiArU5X6M7N6YvDnWSWWuckGIWa2JwkMD9FBgqCbeC9Fx9LkPeeMWv24kL7eU3Vz\nBq5ELQUVz01ZrX/gU4smdetIsargjySK2icHk8k14+ioS20sEf3zB/q778SN85l5\nTJ0TLYsiWS4z/9Yw/4O/ELf/WvuhAxzRAnBKcz6Yw5whlLRcVcf+e+21QryssUYA\nThKIGLGSr3N5EXdGMwAy4JPqthLbRn82WuoT5brK6QqMzvwIm1raQ6t/KNoTDPV/\nPqCa+dpTAgMBAAECggEAJwWREmQfwfRMS4+L4c8Nd4XGavEZKGcxikNM7S0yhBG2\nHMDvhdRK+GGWw1/S8swiahaFel33NMuxdoEbJXNAGIt5/wchQTNlZb9oJjKL2oB0\nRVIuvoKyZZCc46iO6uvxhbZxV2aAXGnxyHvP3TWycfmcRph5fS9elS0kKhUdk7Nl\n0pcn3pfPlazcG6qc4n3feRUOkNvqDt6RzyLnhJMNJbvTmrx8U97CqDPljsHCY6fB\nra8+y3KWGyRdw62m9qWHOBeA9TwaPtdAkot6TdE/0l+GnzQTDQJXNY0z2iltRhG9\nu7UbPvmywBBwjA3c7GvU/JXlR8KXDv0KIhUFD5sgNQKBgQC2Ntr4+dZXBaNbZ+5k\nLnIk1Sjbe/4OzhNCHJ6gvuatpA3f7aRWSmOAIDWcJrhmzbaOk9/yrQaJZrIra5Ox\nwCGS7D9q4CHoEVhCiM7pqoX5HsMFLRL0qdwkosY+T+yH6G+fYgZg/BjFbvhP+3RV\nqZGWC8mUF6YkF29RhVl5ArKvRQKBgQC1EG2oXu4JAdkgrTtPtl5/sb+SJY/exv76\njMjWRYyrXO7BpbAVeEeE3oIsNy6bO1fkKO7NpLwMRie/SkGcIKvGaWsJppzzZvGv\nO47p5GWkjtfmKBhB2Mtm65xGJQRxYQNl5zgiek55gAwzLckllXbGy1s9k4R0cEce\nZez706VQtwKBgFR3YVKBHicA6hT5PL0b+rWwSlxUQhVC2hKPickiNXTQ0822L7QA\nj9dZFwDnwhuFyNaXHf000A7pmDYgjDqdwfKFqXA1rgIR6EQPfzs6XRh6dhT0LBFW\nnEIvYo6IJjFqQjQ0EJjsw97h7iHFgswi6uYPWMZZoB6i7mtv0WYTJhmxAoGAODvL\n8tjY0M9UIgPrQcx/+OS5fKhR0Hy5QBNtZK7hC2+nb1kIIQLkI23/u7+/p9J8b44O\n7KtXA/Dd81kam2TCNLMU3UBzylyUfzneHuIid0Mt5ntZXUn5khNmy5o/kP7yUTnI\ng1y89ptALrzvlc6fvwn1YmBoaMleLSC2w1duJm0CgYEArdAM4l3/Kd6n1meh0wjC\n0ZYInpjQtAjMSEvbUMZmBEd+imJCc7g4CX5PDd4HJf2H8+dH1rBsZVWCvfN9x96e\nEHq4Ul0t7a9pCJwa3m4puXpWnUyF8Fy8IsPpFeFMw40u6Bh9VA1AufhYhq1pCsWy\nG+eMB1Ayl2z1u3CKlAmJMCk=\n-----END PRIVATE KEY-----\n";
