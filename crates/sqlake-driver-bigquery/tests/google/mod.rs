//! A stub standing in for Google, shared by the tests in this directory.
//!
//! Every call this driver makes is HTTPS to a service that bills for some of
//! them, so there is no version of these tests that talks to the real API.
//! What the stub can still prove is the shape of every request the driver
//! sends and what it does with every answer.

// Each integration test binary compiles this file separately, so anything only
// one of them uses is dead code in the others — and `pub` here reaches no
// further than the binary that included it.
#![allow(dead_code, unreachable_pub)]

use sqlake_core::id::ProfileId;
use sqlake_core::profile::{BigQueryAuth, BigQueryParams, Params, ResolvedProfile};
use sqlake_driver_bigquery::BqDriver;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const PROJECT: &str = "analytics-prod";

/// A mock server, and a service-account key file pointing its token endpoint
/// back at it.
pub struct Google {
    server: MockServer,
    // Held, not read: dropping it deletes the key file.
    _dir: tempfile::TempDir,
    key_file: std::path::PathBuf,
}

impl Google {
    pub async fn start() -> Self {
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

    pub fn driver(&self) -> BqDriver {
        BqDriver::new().with_api_url(self.server.uri())
    }

    pub fn key_file(&self) -> std::path::PathBuf {
        self.key_file.clone()
    }

    pub async fn issues_a_token(&self) {
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

    /// A credential the token endpoint will not exchange: a revoked key, or an
    /// `application-default login` that expired.
    pub async fn refuses_a_token(&self) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Invalid grant: account not found",
            })))
            .mount(&self.server)
            .await;
    }

    pub async fn answers_datasets(&self, response: ResponseTemplate) {
        self.answers_datasets_at_page(None, response).await;
    }

    /// The same, for one page of a paged answer. `token` is the `pageToken`
    /// the request must carry, and `None` is the first page — matched on the
    /// parameter being absent so that the pages cannot answer for each other.
    pub async fn answers_datasets_at_page(&self, token: Option<&str>, response: ResponseTemplate) {
        let mock = Mock::given(method("GET"))
            .and(path(format!("/projects/{PROJECT}/datasets")))
            // The token is not decoration: a request without it would be
            // answered by Google with a 401 and by a stub that ignores it with
            // whatever was mounted.
            .and(header("authorization", "Bearer a-token"));
        match token {
            Some(token) => mock.and(query_param("pageToken", token)),
            None => mock.and(query_param_is_missing("pageToken")),
        }
        .respond_with(response)
        .mount(&self.server)
        .await;
    }

    pub async fn answers_tables(&self, dataset: &str, response: ResponseTemplate) {
        self.answers_tables_at_page(dataset, None, response).await;
    }

    pub async fn answers_tables_at_page(
        &self,
        dataset: &str,
        token: Option<&str>,
        response: ResponseTemplate,
    ) {
        let mock = Mock::given(method("GET"))
            .and(path(format!(
                "/projects/{PROJECT}/datasets/{dataset}/tables"
            )))
            .and(header("authorization", "Bearer a-token"));
        match token {
            Some(token) => mock.and(query_param("pageToken", token)),
            None => mock.and(query_param_is_missing("pageToken")),
        }
        .respond_with(response)
        .mount(&self.server)
        .await;
    }

    /// What `tables.get` answers: the schema a row's positional cells are read
    /// against, and the row count the grid shows a position in.
    pub async fn describes_table(
        &self,
        dataset: &str,
        table: &str,
        schema: serde_json::Value,
        num_rows: Option<&str>,
    ) -> &Self {
        let mut body = serde_json::json!({
            "kind": "bigquery#table",
            "id": format!("{PROJECT}:{dataset}.{table}"),
            "tableReference": {
                "projectId": PROJECT,
                "datasetId": dataset,
                "tableId": table,
            },
            "type": "TABLE",
            "schema": schema,
        });
        if let Some(num_rows) = num_rows {
            body["numRows"] = serde_json::json!(num_rows);
        }
        self.describes_table_with(
            dataset,
            table,
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await
    }

    pub async fn describes_table_with(
        &self,
        dataset: &str,
        table: &str,
        response: ResponseTemplate,
    ) -> &Self {
        Mock::given(method("GET"))
            .and(path(format!(
                "/projects/{PROJECT}/datasets/{dataset}/tables/{table}"
            )))
            .and(header("authorization", "Bearer a-token"))
            .respond_with(response)
            .mount(&self.server)
            .await;
        self
    }

    pub async fn answers_rows(&self, dataset: &str, table: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/projects/{PROJECT}/datasets/{dataset}/tables/{table}/data"
            )))
            .and(header("authorization", "Bearer a-token"))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    /// The same, for one window of the table. Matched on `startIndex` and
    /// `maxResults` so that a request for the wrong window is not answered.
    pub async fn answers_rows_at(
        &self,
        dataset: &str,
        table: &str,
        start_index: &str,
        max_results: &str,
        response: ResponseTemplate,
    ) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/projects/{PROJECT}/datasets/{dataset}/tables/{table}/data"
            )))
            .and(header("authorization", "Bearer a-token"))
            .and(query_param("startIndex", start_index))
            .and(query_param("maxResults", max_results))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    pub async fn requests_to_datasets(&self) -> usize {
        self.requests_ending_in("/datasets").await
    }

    pub async fn requests_ending_in(&self, suffix: &str) -> usize {
        self.server
            .received_requests()
            .await
            .expect("the stub records requests")
            .iter()
            .filter(|r| r.url.path().ends_with(suffix))
            .count()
    }
}

pub fn profile(key_file: std::path::PathBuf) -> ResolvedProfile {
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
