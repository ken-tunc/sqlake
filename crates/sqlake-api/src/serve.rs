//! Answering a [`Request`] from a store.
//!
//! The half both front-ends of the agent surface share: one-shot runs it
//! against a store it started itself, and the socket server runs it against the
//! store a person is already using. Neither knows anything the other does not,
//! which is the point — an attached command and a one-shot command differ in
//! where the store came from and nowhere else.
//!
//! Nothing here constructs a query. Every request becomes an `Action` the
//! interactive client also sends, and a wait for the store to settle.

use std::time::Duration;

use sqlake_app::action::Action;
use sqlake_app::snapshot::{LoadState, Snapshot};
use sqlake_app::store::Store;
use sqlake_app::tree::NodeState;
use sqlake_app::wait::WaitError;
use sqlake_core::id::ConnId;
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::result::{Sort, SortDir};

use crate::page::{Budget, Page};
use crate::protocol::{Failure, Request, Response, schema};
use crate::snapshot::{ConnectionInfo, NodeInfo, SessionInfo};

/// How long a request waits for the store before giving up.
///
/// Generous, because the wait is for a database rather than for the process:
/// a cold BigQuery connection or a `gcloud` token refresh can take seconds, and
/// a caller that gave up at one would report a timeout for something that was
/// about to work.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A store, and the policy for reading it.
#[derive(Debug)]
pub struct Service {
    store: Store,
    budget: Budget,
    timeout: Duration,
}

impl Service {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            budget: Budget::DEFAULT,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Answer one request.
    ///
    /// Returns a [`Response`] for every outcome, including the failures: a
    /// request that could not be answered was still delivered and understood,
    /// and a caller distinguishing "no such table" from "the socket died" wants
    /// the first as data.
    pub async fn answer(&self, request: &Request) -> Response {
        match request {
            Request::Schema {} => Response::Schema(schema()),
            Request::Snapshot {} => Response::Snapshot(SessionInfo::from(&*self.store.snapshot())),
            Request::ConnectionList {} => Response::Connections(
                self.store
                    .snapshot()
                    .connections
                    .iter()
                    .map(ConnectionInfo::from)
                    .collect(),
            ),
            Request::NamespaceList { connection } => self
                .namespaces(connection)
                .await
                .unwrap_or_else(Response::Failed),
            Request::TableList {
                connection,
                namespace,
            } => self
                .children(connection, namespace)
                .await
                .unwrap_or_else(Response::Failed),
            Request::TablePreview {
                connection,
                table,
                sort,
                ..
            } => self
                .preview(
                    connection,
                    table,
                    sort.map(|s| s.column),
                    request.budget(self.budget),
                )
                .await
                .unwrap_or_else(Response::Failed),
        }
    }

    /// The connection this request names, once it has finished opening.
    ///
    /// Resolved by comparing the id it prints rather than by parsing one: an id
    /// that is well formed but not open and an id that is not an id at all are
    /// the same answer to the caller, and only one of the two would survive a
    /// parse.
    async fn connection(&self, id: &str) -> Result<ConnId, Failure> {
        let conn = self
            .store
            .snapshot()
            .connections
            .iter()
            .find(|c| c.id.to_string() == id)
            .map(|c| c.id)
            .ok_or_else(|| Failure::NoSuchConnection {
                connection: id.to_owned(),
            })?;
        let settled = self.settle(|s| s.connection_settled(conn)).await?;
        match settled.connection(conn).map(|c| &c.status) {
            Some(sqlake_app::snapshot::ConnStatus::Failed(why)) => Err(Failure::Driver {
                message: why.clone(),
            }),
            // A closed connection has no session, so every action below it is
            // dropped by the store and the request would report the tree as
            // empty and the table as missing.
            Some(sqlake_app::snapshot::ConnStatus::Closed) => Err(Failure::Driver {
                message: "the connection is closed".to_owned(),
            }),
            _ => Ok(conn),
        }
    }

    async fn namespaces(&self, connection: &str) -> Result<Response, Failure> {
        let conn = self.connection(connection).await?;
        // A connection's first level arrives with `Connect`, so this is a read
        // rather than a fetch.
        let snapshot = self.store.snapshot();
        Ok(Response::Nodes(
            snapshot
                .objects(conn)
                .filter(|n| n.node_ref.depth() == 1)
                .map(NodeInfo::from)
                .collect(),
        ))
    }

    async fn children(&self, connection: &str, namespace: &[String]) -> Result<Response, Failure> {
        let conn = self.connection(connection).await?;
        // The node is taken from the tree rather than built from the path: its
        // `NodeKind` is the driver's, and guessing one here would be this crate
        // deciding whether a level is a schema or a dataset.
        let node = self.resolve(conn, namespace).await?;
        // A relation has no children, so expanding one answers "there is
        // nothing in here" — which is what an empty namespace says too, and
        // the caller cannot tell the two apart.
        if node.as_table().is_some() {
            return Err(Failure::Unsupported {
                message: format!("{} is a relation, not a namespace", namespace.join(".")),
            });
        }
        let settled = self
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: node.clone(),
                },
                |s| s.node_settled(conn, &node),
            )
            .await?;
        expanded(&settled, conn, &node)?;
        Ok(Response::Nodes(
            settled
                .objects(conn)
                .filter(|n| n.node_ref.path.len() == namespace.len() + 1)
                .filter(|n| n.node_ref.path.starts_with(namespace))
                .map(NodeInfo::from)
                .collect(),
        ))
    }

    async fn preview(
        &self,
        connection: &str,
        path: &[String],
        sort: Option<usize>,
        budget: Budget,
    ) -> Result<Response, Failure> {
        let conn = self.connection(connection).await?;
        let table =
            self.resolve(conn, path)
                .await?
                .as_table()
                .ok_or_else(|| Failure::Unsupported {
                    message: format!("{} is not a relation", path.join(".")),
                })?;

        let settled = self
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: table.clone(),
                },
                |s| s.preview_settled(conn, &table),
            )
            .await?;
        let settled = match sort {
            Some(column) => self.sorted(conn, &table, column, settled).await?,
            None => settled,
        };

        match settled.preview(conn, &table).map(|p| &p.data) {
            Some(LoadState::Ready(result)) => Ok(Response::Page(Page::of(result, budget))),
            Some(LoadState::Failed(why)) => Err(Failure::Driver {
                message: why.clone(),
            }),
            // Settled but neither ready nor failed: the preview was forgotten
            // under this caller, which on a shared session is a person closing
            // the tab it was reading.
            _ => Err(Failure::NotFound {
                path: path.to_vec(),
            }),
        }
    }

    /// Sorting is a second action against a preview that already exists,
    /// because `SortPreview` sorts what is loaded rather than fetching.
    ///
    /// The store toggles rather than taking a direction, so a request asking
    /// for a column the preview is already sorted by would reverse it. Asked
    /// for once, ascending is what a caller with no previous state means.
    async fn sorted(
        &self,
        conn: ConnId,
        table: &TableRef,
        column: usize,
        settled: std::sync::Arc<Snapshot>,
    ) -> Result<std::sync::Arc<Snapshot>, Failure> {
        let wanted = Sort::new(column, SortDir::Asc);
        if settled.preview(conn, table).and_then(|p| p.sort) == Some(wanted) {
            return Ok(settled);
        }
        let settled = self
            .dispatch_and_settle(
                Action::SortPreview {
                    conn,
                    table: table.clone(),
                    column,
                },
                |s| s.preview_settled(conn, table),
            )
            .await?;

        // The store drops a sort it cannot perform — an unsortable connection,
        // a column index no page has — without recording anything, so the wait
        // above ends on rows in whatever order they already had. Sending those
        // back as a success answers a question nobody asked, and [`Page`]
        // carries no ordering for the caller to notice it by.
        if settled.preview(conn, table).and_then(|p| p.sort) != Some(wanted) {
            return Err(Failure::Unsupported {
                message: if settled
                    .connection(conn)
                    .and_then(|c| c.capabilities.as_ref())
                    .is_some_and(|c| !c.sortable_preview)
                {
                    "this connection cannot order a preview".to_owned()
                } else {
                    format!("there is no column {column} to sort {table} by")
                },
            });
        }
        Ok(settled)
    }

    /// The node at this path, loading whatever has to be loaded to reach it.
    ///
    /// A caller names a path; it does not replay the clicks a person would have
    /// made to bring that path onto a screen. Only the first level arrives with
    /// `Connect`, so `public.users` is not in the tree until `public` has been
    /// expanded — and requiring a `table_list` first would make every preview a
    /// two-call sequence whose first call the caller does not want the answer
    /// to.
    ///
    /// `ExpandNode` rather than `ToggleNode` is what makes this safe to repeat:
    /// an ancestor somebody already opened stays open (D2).
    async fn resolve(&self, conn: ConnId, path: &[String]) -> Result<NodeRef, Failure> {
        for depth in 1..path.len() {
            let ancestor = self.node(conn, &path[..depth])?;
            let settled = self
                .dispatch_and_settle(
                    Action::ExpandNode {
                        conn,
                        node: ancestor.clone(),
                    },
                    |s| s.node_settled(conn, &ancestor),
                )
                .await?;
            // Checked before descending: an ancestor the driver refused has no
            // children, so the next lookup reports the path as missing and
            // hides the refusal behind a plausible "not found".
            expanded(&settled, conn, &ancestor)?;
        }
        self.node(conn, path)
    }

    fn node(&self, conn: ConnId, path: &[String]) -> Result<NodeRef, Failure> {
        self.store
            .snapshot()
            .objects(conn)
            .find(|n| n.node_ref.path == path)
            .map(|n| n.node_ref.clone())
            .ok_or_else(|| Failure::NotFound {
                path: path.to_vec(),
            })
    }

    async fn settle(
        &self,
        done: impl Fn(&Snapshot) -> bool,
    ) -> Result<std::sync::Arc<Snapshot>, Failure> {
        self.store
            .settle(self.timeout, done)
            .await
            .map_err(|e| self.waited(e))
    }

    async fn dispatch_and_settle(
        &self,
        action: Action,
        done: impl Fn(&Snapshot) -> bool,
    ) -> Result<std::sync::Arc<Snapshot>, Failure> {
        self.store
            .dispatch_and_settle(action, self.timeout, done)
            .await
            .map_err(|e| self.waited(e))
    }

    fn waited(&self, error: WaitError) -> Failure {
        match error {
            WaitError::TimedOut => Failure::Timeout {
                waited_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
            },
            WaitError::Stopped => Failure::Driver {
                message: "the session has stopped".to_owned(),
            },
        }
    }
}

/// Whether an expansion that has settled actually loaded anything.
///
/// A node the driver refused settles with no children, which is on the wire
/// indistinguishable from a namespace that is genuinely empty — so a caller
/// told `[]` concludes the schema has no tables when it was denied permission
/// to look. The reason is on the node, where the TUI draws it.
fn expanded(snapshot: &Snapshot, conn: ConnId, node: &NodeRef) -> Result<(), Failure> {
    match snapshot
        .objects(conn)
        .find(|n| &n.node_ref == node)
        .map(|n| &n.state)
    {
        Some(NodeState::Failed(why)) => Err(Failure::Driver {
            message: why.clone(),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde_json::Value as Json;
    use sqlake_app::store::Drivers;
    use sqlake_core::id::ProfileId;
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::protocol::{FailureKind, ResponseKind, SortBy};

    async fn service(behaviour: Behaviour) -> (Service, String) {
        service_of(MockDriver::new(behaviour)).await
    }

    async fn service_of(driver: MockDriver) -> (Service, String) {
        let store = Store::spawn(
            Drivers::new().with(Arc::new(driver)),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
        );
        let conn = ConnId::new();
        store
            .dispatch_and_settle(
                Action::Connect {
                    profile: ProfileId::parse("mock").expect("a usable id"),
                    conn,
                },
                DEFAULT_TIMEOUT,
                |s| s.connection_settled(conn),
            )
            .await
            .expect("the connection settles");
        (Service::new(store), conn.to_string())
    }

    fn nodes(response: &Response) -> &[NodeInfo] {
        match response {
            Response::Nodes(nodes) => nodes,
            other => panic!("expected nodes, got {other:?}"),
        }
    }

    fn page(response: &Response) -> &Page {
        match response {
            Response::Page(page) => page,
            other => panic!("expected a page, got {other:?}"),
        }
    }

    /// Every key in every response that can cross the socket.
    ///
    /// A1's fifth promise is that nothing crossing the socket carries a host, a
    /// user, or anything derived from a credential — and everything crossing it
    /// is a `Response`. Asserted as the whole key set rather than as the
    /// absence of a list of bad names: a field added anywhere below a response
    /// fails this and has to be looked at, which is the only version of the
    /// check that keeps working.
    ///
    /// Coverage is two levels deep. `Failed` is not one shape — each `Failure`
    /// carries its own fields — so sampling one failure looks at a sixth of
    /// what `Failed` can write. And `Malformed` is never produced by
    /// [`Service::answer`] at all, so provoking failures is not a way to
    /// enumerate them: the last of each kind is constructed, which serialises
    /// identically to one that was earned.
    #[tokio::test]
    async fn no_response_carries_a_credential() {
        fn keys(value: &Json, into: &mut BTreeSet<String>) {
            match value {
                Json::Object(members) => {
                    for (key, nested) in members {
                        into.insert(key.clone());
                        keys(nested, into);
                    }
                }
                Json::Array(items) => items.iter().for_each(|v| keys(v, into)),
                _ => {}
            }
        }

        let (session, conn) = service(Behaviour::instant()).await;
        // One column, so `omitted_columns` is written rather than skipped —
        // every `skip_serializing_if` field below a response is a field this
        // check does not see unless the sample makes it appear.
        let narrow = Budget {
            max_rows: 1,
            max_columns: 1,
        };
        let (refusing, refused_conn) = service(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        })
        .await;

        let mut responses = vec![
            session.answer(&Request::Snapshot {}).await,
            session.answer(&Request::ConnectionList {}).await,
            // Relations, so `NodeInfo::relation_kind` is written: it is skipped
            // on a namespace, and a sample of only namespaces would not see it.
            session
                .answer(&Request::TableList {
                    connection: conn.clone(),
                    namespace: vec!["public".into()],
                })
                .await,
            // A namespace that failed, so `NodeInfo::error` carries a driver's
            // own words — the one place they reach the wire.
            refusing
                .answer(&Request::NamespaceList {
                    connection: refused_conn.clone(),
                })
                .await,
            refusing
                .answer(&Request::TableList {
                    connection: refused_conn,
                    namespace: vec!["restricted".into()],
                })
                .await,
        ];
        responses.push(
            Service::new(session.store().clone())
                .with_budget(narrow)
                .answer(&Request::TablePreview {
                    connection: conn,
                    table: vec!["public".into(), "users".into()],
                    sort: None,
                    limit: None,
                })
                .await,
        );

        // A connection that failed to open, for `Status::Failed`'s reason.
        let (broken, _) = service(Behaviour {
            connect_fails: true,
            ..Behaviour::instant()
        })
        .await;
        responses.push(broken.answer(&Request::Snapshot {}).await);

        // One of every failure, so `Failed`'s own coverage is total.
        for failure in [
            Failure::NoSuchConnection {
                connection: "id".into(),
            },
            Failure::NotFound {
                path: vec!["public".into()],
            },
            Failure::Driver {
                message: "refused".into(),
            },
            Failure::Timeout { waited_ms: 1 },
            Failure::Unsupported {
                message: "cannot".into(),
            },
            Failure::Malformed {
                message: "not a request".into(),
            },
        ] {
            responses.push(Response::Failed(failure));
        }
        // The schema is a response too, and is left out of the key set on
        // purpose: it describes the protocol, so its keys are every field name
        // in this crate and would swamp what this is looking at.
        responses.push(session.answer(&Request::Schema {}).await);

        assert_eq!(
            responses
                .iter()
                .map(Response::kind)
                .collect::<BTreeSet<_>>(),
            ResponseKind::ALL.iter().copied().collect::<BTreeSet<_>>(),
            "a response this surface can send is not exercised here"
        );
        assert_eq!(
            responses
                .iter()
                .filter_map(|r| match r {
                    Response::Failed(f) => Some(f.kind()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>(),
            FailureKind::ALL.iter().copied().collect::<BTreeSet<_>>(),
            "a failure this surface can send is not exercised here"
        );

        let mut found = BTreeSet::new();
        for response in &mut responses {
            match response {
                Response::Schema(_) => continue,
                // Cell values are the caller's data, not protocol structure. A
                // `STRUCT` column would otherwise put its field names in here,
                // which makes this assertion a statement about the fixture.
                Response::Page(page) => page.rows.clear(),
                _ => {}
            }
            keys(
                &serde_json::to_value(&*response).expect("a response serialises"),
                &mut found,
            );
        }

        let expected: BTreeSet<String> = [
            "cancel",
            "capabilities",
            "columns",
            "connection",
            "connections",
            "cost_estimate",
            "data",
            "driver",
            "error",
            "free_preview",
            "hierarchy",
            "id",
            "loaded",
            "message",
            "name",
            "nullable",
            "omitted_columns",
            "path",
            "profile",
            "profiles",
            "reason",
            "relation_kind",
            "response",
            "returned",
            "rows",
            "sortable_preview",
            "state",
            "status",
            "total",
            "truncated",
            "type_name",
            "waited_ms",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn the_schema_needs_no_connection() {
        let (service, _) = service(Behaviour::instant()).await;
        let answer = service.answer(&Request::Schema {}).await;
        assert!(matches!(answer, Response::Schema(_)));
    }

    #[tokio::test]
    async fn an_unknown_connection_is_a_failure_rather_than_a_wait() {
        let (service, _) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::NamespaceList {
                connection: "not-an-id".into(),
            })
            .await;
        assert_eq!(
            answer,
            Response::Failed(Failure::NoSuchConnection {
                connection: "not-an-id".into()
            })
        );
    }

    #[tokio::test]
    async fn the_namespaces_are_the_first_level_whatever_the_driver_calls_it() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::NamespaceList { connection: conn })
            .await;
        let names: Vec<&str> = nodes(&answer).iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"public"), "{names:?}");
        assert!(
            nodes(&answer).iter().all(|n| n.path.len() == 1),
            "a deeper node was reported as a namespace"
        );
    }

    #[tokio::test]
    async fn listing_a_namespace_fetches_it() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TableList {
                connection: conn,
                namespace: vec!["public".into()],
            })
            .await;
        let names: Vec<&str> = nodes(&answer).iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"users"), "{names:?}");
    }

    #[tokio::test]
    async fn a_namespace_that_is_not_there_is_not_a_timeout() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TableList {
                connection: conn,
                namespace: vec!["nowhere".into()],
            })
            .await;
        assert_eq!(
            answer,
            Response::Failed(Failure::NotFound {
                path: vec!["nowhere".into()]
            })
        );
    }

    #[tokio::test]
    async fn a_preview_reads_a_page() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                sort: None,
                limit: None,
            })
            .await;
        let page = page(&answer);
        assert!(!page.rows.is_empty());
        assert_eq!(page.returned, page.rows.len());
    }

    #[tokio::test]
    async fn a_preview_needs_no_listing_first() {
        // Only the first level arrives with `Connect`, so `public.users` is not
        // in the tree until `public` is expanded. Requiring the caller to ask
        // for a listing it does not want makes every preview two calls, and the
        // failure without it looks like the table is missing.
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                sort: None,
                limit: Some(1),
            })
            .await;
        assert!(!page(&answer).rows.is_empty(), "{answer:?}");
    }

    #[tokio::test]
    async fn a_limit_cuts_the_page_and_the_page_says_so() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                sort: None,
                limit: Some(3),
            })
            .await;
        let page = page(&answer);
        assert_eq!(page.returned, 3);
        assert!(page.truncated, "a cut page did not say it was cut");
        assert!(page.loaded > 3, "nothing was actually left out");
    }

    #[tokio::test]
    async fn a_sort_asked_for_once_is_ascending() {
        let (service, conn) = service(Behaviour::instant()).await;
        let request = Request::TablePreview {
            connection: conn,
            table: vec!["public".into(), "users".into()],
            sort: Some(SortBy { column: 1 }),
            limit: Some(5),
        };
        let first = page(&service.answer(&request).await).clone();
        // Asked for again, it must not toggle: the store sorts by toggling, and
        // a caller with no previous state means "ascending" both times.
        let again = page(&service.answer(&request).await).clone();
        assert_eq!(first.rows, again.rows);
    }

    #[tokio::test]
    async fn previewing_something_that_is_not_a_relation_says_so() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into()],
                sort: None,
                limit: None,
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Unsupported { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn a_driver_that_fails_reports_its_message_rather_than_timing_out() {
        let (service, conn) = service(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        })
        .await;
        let answer = service
            .answer(&Request::TableList {
                connection: conn,
                namespace: vec!["restricted".into()],
            })
            .await;
        // Not an empty list: a namespace with no tables and one the driver
        // refused to open are the same answer otherwise, and the second is the
        // one an agent must not act on.
        assert!(
            matches!(answer, Response::Failed(Failure::Driver { .. })),
            "a refused expansion was reported as an empty namespace: {answer:?}"
        );
    }

    #[tokio::test]
    async fn a_sort_the_connection_cannot_do_is_refused_rather_than_ignored() {
        let (service, conn) = service_of(
            MockDriver::new(Behaviour::instant()).with_capabilities(sqlake_driver_mock::NO_SORT),
        )
        .await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                sort: Some(SortBy { column: 1 }),
                limit: None,
            })
            .await;
        // The store drops a sort it cannot perform. Answering with the rows in
        // their original order looks like success, and `Page` says nothing
        // about ordering for the caller to catch it by.
        assert!(
            matches!(answer, Response::Failed(Failure::Unsupported { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn a_sort_by_a_column_that_is_not_there_is_refused() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TablePreview {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                sort: Some(SortBy { column: 9_999 }),
                limit: None,
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Unsupported { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn listing_the_children_of_a_relation_says_it_is_one() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TableList {
                connection: conn,
                namespace: vec!["public".into(), "users".into()],
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Unsupported { .. })),
            "a relation was reported as an empty namespace: {answer:?}"
        );
    }
}
