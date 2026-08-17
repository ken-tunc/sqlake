//! The BigQuery driver.
//!
//! The rule this crate follows above all others is that **reading must not
//! cost money**. Browsing and previewing go through the free REST endpoints —
//! `datasets.list`, `tables.list`, `tabledata.list` — and never through a
//! query. A driver that reached for `SELECT *` here would bill a scan for
//! every click on a table.
//!
//! That is also why [`CAPABILITIES`] answers `free_preview` true and
//! `sortable_preview` false: `tabledata.list` is not billed and cannot order
//! rows, and the only way to order them is the thing that costs.

pub mod catalog;
pub mod error;

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use gcp_bigquery_client::Client;
use gcp_bigquery_client::client_builder::ClientBuilder;
use gcp_bigquery_client::dataset::ListOptions;
use sqlake_core::capability::{Capabilities, DriverKind, HierarchyLevel, QuoteStyle};
use sqlake_core::driver::{Driver, DriverError, DriverResult, Session};
use sqlake_core::node::{NodeKind, NodeRef, TableRef, TreeNode};
use sqlake_core::profile::{BigQueryAuth, BigQueryParams, Params, ResolvedProfile};
use sqlake_core::result::{PageRequest, ResultSet};

use crate::error::{connect_failed, driver_error, is_empty_dataset_list};

/// Project, dataset, table — the same three levels PostgreSQL has, under
/// different names. Keeping the names in the level list is what lets the tree
/// say "dataset" without a `match` in the UI.
pub const HIERARCHY: &[HierarchyLevel] = &[
    HierarchyLevel::new(NodeKind::Catalog, "project"),
    HierarchyLevel::new(NodeKind::Namespace, "dataset"),
    HierarchyLevel::new(NodeKind::Relation, "table"),
];

pub const CAPABILITIES: Capabilities = Capabilities {
    hierarchy: HIERARCHY,
    indexes: false,
    triggers: false,
    constraints: false,
    // The one physical-layout feature BigQuery has, and the one that decides
    // what a query costs.
    partitioning: true,
    transactions: false,
    cancel: true,
    streaming: true,
    // `jobs.insert` with `dryRun` returns the byte estimate and runs nothing.
    cost_estimate: true,
    free_preview: true,
    sortable_preview: false,
    quote_style: QuoteStyle::Backtick,
};

/// How long a call to Google has to answer.
///
/// Every call this driver makes is HTTPS and `reqwest` has no deadline of its
/// own: without this, a machine behind a proxy that accepts connections and
/// answers nothing leaves the caller spinning with no way to abandon it. That
/// costs more after the connection is open than during it — one session actor
/// serialises every request for a connection, so a listing that never returns
/// takes the connection's previews and every other branch of its tree with it.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// The BigQuery REST API. Overridden in tests, where the whole point is that
/// no request leaves the machine.
const API_URL: &str = "https://bigquery.googleapis.com/bigquery/v2";

/// The OAuth scope a token is asked for.
///
/// Read-write even for a profile whose `readonly` is set, which the PostgreSQL
/// driver does honour: the narrower `…/bigquery.readonly` scope cannot create a
/// job, and *reading* a table with SQL is a job, so a token with it could not
/// run the `SELECT` the read-only profile exists to allow. A read-only BigQuery
/// connection is therefore a job the profile's own IAM role does, and M4's
/// refusal of a statement that writes — not something a scope can express.
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

#[derive(Debug)]
pub struct BqDriver {
    api_url: String,
    deadline: Duration,
}

impl Default for BqDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl BqDriver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_url: API_URL.to_owned(),
            deadline: DEADLINE,
        }
    }

    /// Point the driver at something other than Google.
    ///
    /// The only way this crate can be tested at all: every call it makes is an
    /// HTTPS request to a service that bills for some of them, so the tests
    /// run against a stub and the endpoint has to be a value rather than a
    /// constant.
    #[must_use]
    pub fn with_api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Acquire credentials and build a client around them.
    ///
    /// Both arms end in the same place; what differs is where the token comes
    /// from. ADC is the discovery chain `gcloud` writes into and the metadata
    /// server answers on, so it works unchanged inside Cloud Run and on a
    /// laptop, and it is why the profile does not have to name a file at all.
    async fn client(&self, params: &BigQueryParams) -> DriverResult<Client> {
        let mut builder = ClientBuilder::new();
        let builder = builder
            .with_v2_base_url(self.api_url.clone())
            .with_auth_base_url(SCOPE.to_owned());

        match &params.auth {
            BigQueryAuth::Adc => builder
                .build_from_application_default_credentials()
                .await
                .map_err(connect_failed),
            BigQueryAuth::ServiceAccount(path) => {
                // `sqlake-config` has already refused a relative path and a
                // `~`, so a path that does not resolve here is a file that is
                // not there rather than one that was written ambiguously.
                let path = path.to_str().ok_or_else(|| {
                    DriverError::Connect(format!(
                        "the service account key path is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
                builder
                    .build_from_service_account_key_file(path)
                    .await
                    .map_err(connect_failed)
            }
        }
    }
}

#[async_trait]
impl Driver for BqDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::BigQuery
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn connect(&self, profile: &ResolvedProfile) -> DriverResult<Box<dyn Session>> {
        let Params::BigQuery(params) = &profile.params else {
            return Err(DriverError::Connect(format!(
                "profile `{}` is not a bigquery profile",
                profile.id
            )));
        };

        let opening = async {
            let client = self.client(params).await?;
            verify(&client, &params.project).await?;
            Ok::<_, DriverError>(client)
        };

        // BigQuery has no connection to open: building a client only reads a
        // file, so without the call below a profile naming a project nobody
        // has access to would connect happily and fail on the first click.
        let client = tokio::time::timeout(self.deadline, opening)
            .await
            // Google, not `api_url`: the token exchange is under this deadline
            // too and goes to the credential's own endpoint — the metadata
            // server, when ADC is what answers — so naming the API URL would
            // send someone to check the host that was still responding.
            .map_err(|_| {
                DriverError::Connect(format!("no answer from Google within {:?}", self.deadline))
            })??;

        Ok(Box::new(BqSession {
            client,
            project: params.project.clone(),
            deadline: self.deadline,
        }))
    }
}

/// One call, chosen because it fails differently for each thing that can be
/// wrong: bad credentials answer 401, a project the caller cannot see answers
/// 403, and one that does not exist answers 404. Asking for a single dataset
/// keeps it cheap on a project with thousands.
///
/// An empty project is the one failure forgiven — see
/// [`is_empty_dataset_list`]. Nothing else is, and the difference matters most
/// for the failure that does not look like one: the token is fetched here, not
/// while the client is built, so a revoked key and an ADC login that expired
/// both arrive as an authentication error from this call. Treating those as
/// "ask again later" would report a connection that can never answer anything.
async fn verify(client: &Client, project: &str) -> DriverResult<()> {
    match client
        .dataset()
        .list(project, ListOptions::default().max_results(1))
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if is_empty_dataset_list(&err) => {
            tracing::debug!(error = %err, %project, "the dataset list did not parse");
            Ok(())
        }
        Err(other) => Err(connect_failed(other)),
    }
}

pub struct BqSession {
    client: Client,
    project: String,
    deadline: Duration,
}

impl BqSession {
    /// Give up on a call to Google that is not going to answer.
    ///
    /// The whole of a listing rather than each page: what the caller is
    /// waiting for is the branch, and a first page that arrives inside the
    /// deadline is no comfort if the second never does.
    async fn within_deadline<T>(
        &self,
        what: &str,
        work: impl Future<Output = DriverResult<T>>,
    ) -> DriverResult<T> {
        tokio::time::timeout(self.deadline, work)
            .await
            .map_err(|_| {
                driver_error(format!(
                    "no answer from Google within {:?} while {what}",
                    self.deadline
                ))
            })?
    }
}

/// Hand-written because `Client` is not `Debug` — and it should stay
/// hand-written if that ever changes: what it holds is an authenticator, and
/// a session is logged by `tracing` wherever one appears.
impl std::fmt::Debug for BqSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BqSession")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Session for BqSession {
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn children(&self, of: &NodeRef) -> DriverResult<Vec<TreeNode>> {
        self.within_deadline(
            &format!("expanding `{of}`"),
            catalog::children(&self.client, &self.project, of),
        )
        .await
    }

    /// T4.
    async fn preview(&self, table: &TableRef, _req: &PageRequest) -> DriverResult<ResultSet> {
        Err(driver_error(format!("reading {table} is not built yet")))
    }

    /// Nothing to release: `reqwest` owns a connection pool that drops with the
    /// client, and a token is a string that expires on its own.
    async fn close(self: Box<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserting the fields of [`CAPABILITIES`] would only restate the literal
    /// above; what is worth checking is that the level list answers a lookup,
    /// because that is how the tree gets the word "dataset" onto the screen
    /// without a `match` on the driver.
    #[test]
    fn the_hierarchy_answers_to_what_bigquery_calls_things() {
        assert_eq!(CAPABILITIES.label_for(NodeKind::Namespace), Some("dataset"));
        assert_eq!(CAPABILITIES.label_for(NodeKind::Root), None);
    }
}
