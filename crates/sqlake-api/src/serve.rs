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
use sqlake_app::snapshot::{BusyOwner, LoadState, QueryView, Snapshot};
use sqlake_app::store::Store;
use sqlake_app::tree::NodeState;
use sqlake_app::wait::WaitError;
use std::collections::BTreeMap;

use sqlake_core::capability::{Escaping, QuoteStyle};
use sqlake_core::id::{ConnId, QueryId};
use sqlake_core::library::Template;
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::result::{Sort, SortDir};
use sqlake_core::template::{BoundTemplate, Dialect};

use crate::page::{Budget, Page};
use crate::protocol::{Failure, Request, Response, schema};
use crate::snapshot::{
    ConnectionInfo, DefinitionInfo, NodeInfo, QueryInfo, SessionInfo, StatementInfo, TemplateInfo,
};

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
    /// Bytes a query may cost before this surface refuses it, or `None` to
    /// leave it to the session's own ceiling.
    ///
    /// Below the session's, and applied on top of it — never instead. A person
    /// over their limit is shown a dialog and answers it; an agent over this
    /// one cannot answer at all, which is why it is meant to be the lower.
    max_bytes: Option<u64>,
    timeout: Duration,
}

impl Service {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            budget: Budget::DEFAULT,
            max_bytes: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub const fn with_max_bytes(mut self, max_bytes: Option<u64>) -> Self {
        self.max_bytes = max_bytes;
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
            Request::ConnectionOpen { profile } => {
                self.open(profile).await.unwrap_or_else(Response::Failed)
            }
            Request::ConnectionClose { connection } => self
                .close(connection)
                .await
                .unwrap_or_else(Response::Failed),
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
            Request::QueryEstimate { connection, sql } => self
                .estimate_query(connection, sql)
                .await
                .unwrap_or_else(Response::Failed),
            Request::QueryRun {
                connection,
                sql,
                max_bytes,
            } => self
                .run_query(connection, sql, self.max_bytes(*max_bytes))
                .await
                .unwrap_or_else(Response::Failed),
            Request::QueryStatus { query, .. } => self
                .query_status(query, request.budget(self.budget))
                .await
                .unwrap_or_else(Response::Failed),
            Request::QueryWait {
                query, timeout_ms, ..
            } => self
                .query_wait(query, *timeout_ms, request.budget(self.budget))
                .await
                .unwrap_or_else(Response::Failed),
            Request::QueryCancel { query } => self
                .query_cancel(query)
                .await
                .unwrap_or_else(Response::Failed),
            Request::TemplateList {} => self.templates().await.unwrap_or_else(Response::Failed),
            Request::TemplateApply {
                template,
                values,
                connection,
            } => self
                .apply_template(template, values, connection.as_deref())
                .await
                .unwrap_or_else(Response::Failed),
            Request::TableDescribe {
                connection,
                table,
                refresh,
            } => self
                .describe(connection, table, *refresh, request.budget(self.budget))
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

    /// The connection this request names, whatever state it is in.
    ///
    /// Resolved by comparing the id it prints rather than by parsing one: an id
    /// that is well formed but not open and an id that is not an id at all are
    /// the same answer to the caller, and only one of the two would survive a
    /// parse.
    fn conn_id(&self, id: &str) -> Result<ConnId, Failure> {
        self.store
            .snapshot()
            .connections
            .iter()
            .find(|c| c.id.to_string() == id)
            .map(|c| c.id)
            .ok_or_else(|| Failure::NoSuchConnection {
                connection: id.to_owned(),
            })
    }

    /// The connection this request names, once it has finished opening, and
    /// only if it is in a state that can answer.
    async fn connection(&self, id: &str) -> Result<ConnId, Failure> {
        let conn = self.conn_id(id)?;
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

    /// Open a connection, and wait for it to finish opening.
    ///
    /// Waited on rather than answered straight away: `dispatch` only queues, so
    /// returning the id immediately would hand the caller something no snapshot
    /// has yet — and the next request would be told there is no such
    /// connection, about the one it was just given.
    ///
    /// A connection that failed to open is still answered as a connection
    /// rather than as an error. It exists, its row says why, and closing it is
    /// something the caller may want to do.
    async fn open(&self, profile: &str) -> Result<Response, Failure> {
        let snapshot = self.store.snapshot();
        let profile = snapshot
            .profiles
            .iter()
            .find(|p| p.id.as_str() == profile)
            .map(|p| p.id.clone())
            .ok_or_else(|| Failure::NoSuchProfile {
                profile: profile.to_owned(),
            })?;

        // The id is chosen here for the same reason `Action::Connect` takes
        // one: two connections may name the same profile, so "the one that was
        // not there before" is not something to read out of a snapshot a person
        // is also changing.
        let conn = ConnId::new();
        let settled = self
            .dispatch_and_settle(Action::Connect { profile, conn }, move |s| {
                s.connection_settled(conn)
            })
            .await?;
        settled
            .connection(conn)
            .map(|c| Response::Connection(ConnectionInfo::from(c)))
            .ok_or_else(|| Failure::NoSuchConnection {
                connection: conn.to_string(),
            })
    }

    /// Close a connection, and answer with what it became.
    ///
    /// Not [`Self::connection`], which refuses an already-closed one: closing
    /// something twice is a caller repeating itself, and the honest answer is
    /// the connection, closed.
    async fn close(&self, connection: &str) -> Result<Response, Failure> {
        let conn = self.conn_id(connection)?;
        let settled = self
            .dispatch_and_settle(Action::Disconnect(conn), move |s| {
                s.connection(conn)
                    .is_none_or(|c| c.status == sqlake_app::snapshot::ConnStatus::Closed)
            })
            .await?;
        settled
            .connection(conn)
            .map(|c| Response::Connection(ConnectionInfo::from(c)))
            .ok_or_else(|| Failure::NoSuchConnection {
                connection: connection.to_owned(),
            })
    }

    /// Cost a statement and stop there.
    ///
    /// Waited on, unlike running: there is nothing to hold a handle for. The
    /// answer is one number and it is the whole point of the call.
    async fn estimate_query(&self, connection: &str, sql: &str) -> Result<Response, Failure> {
        let conn = self.connection(connection).await?;
        let query = QueryId::new();
        let costed = self
            .dispatch_and_settle(
                Action::EstimateQuery {
                    conn,
                    query,
                    sql: sql.to_owned(),
                },
                move |s| s.query(query).is_some_and(QueryView::is_settled),
            )
            .await
            .and_then(|settled| self.query_info(&settled, query));
        // Forgotten afterwards: an estimate leaves nothing to come back for,
        // and a session accumulating one `QueryView` per question an agent
        // asked is a leak with a plausible-looking cause. Waited on so that it
        // is gone by the time this answers, rather than a moment later.
        //
        // Unconditionally, before anything above is allowed to return: an
        // estimate that outlives the wait is exactly the case that leaves a
        // `QueryView` — and a busy row reading "estimating a query" — behind
        // for ever. `ForgetQuery` drops the task too, so nothing is still in
        // flight to land on it.
        let forgotten = self
            .dispatch_and_settle(Action::ForgetQuery(query), move |s| {
                s.query(query).is_none()
            })
            .await;
        let answer = costed?;
        forgotten?;
        Ok(Response::Query(answer))
    }

    /// Start a query and answer with whatever state it is in.
    ///
    /// Not waited on: a query is not instantaneous, and a caller blocking on a
    /// socket read for four minutes is a poor client. What it waits for is the
    /// store *accepting* the action, so the id it is handed is one the next
    /// request can ask about.
    ///
    /// A query fast enough to have finished by then is answered as finished,
    /// rows and all. That is not a contradiction of "answers a handle rather
    /// than rows": it does not *wait* for rows, and hiding ones it already has
    /// would be a second request to fetch what was in hand. There is no limit
    /// on this request to cut them by, so the service's own budget does.
    async fn run_query(
        &self,
        connection: &str,
        sql: &str,
        max_bytes: Option<u64>,
    ) -> Result<Response, Failure> {
        let conn = self.connection(connection).await?;
        let query = QueryId::new();
        let settled = self
            .dispatch_and_settle(
                Action::RunQuery {
                    conn,
                    query,
                    sql: sql.to_owned(),
                    // Not the caller's row budget. The store's page is shared
                    // — a person may be looking at this query in the session
                    // too — and capping the fetch would make them inherit an
                    // agent's context window. The budget cuts what is *written
                    // out*, which is what `Page::of` does below.
                    max_rows: None,
                    max_bytes,
                },
                move |s| s.query(query).is_some(),
            )
            .await?;
        // The service's own budget, not a request's: this answers a handle
        // and no rows, so there is nothing for a limit to cut.
        Ok(Response::Query(QueryInfo::of(
            self.query(&settled, query)?,
            self.budget,
        )))
    }

    /// The tighter of this surface's ceiling and the caller's.
    ///
    /// Only ever downward, the way a page limit is. The store applies the
    /// session's on top of whatever comes out of here, so the effective
    /// ceiling is the lowest of the three.
    fn max_bytes(&self, asked: Option<u64>) -> Option<u64> {
        match (self.max_bytes, asked) {
            (Some(ours), Some(asked)) => Some(ours.min(asked)),
            (ours, asked) => ours.or(asked),
        }
    }

    async fn query_status(&self, query: &str, budget: Budget) -> Result<Response, Failure> {
        let snapshot = self.store.snapshot();
        let id = self.query_id(&snapshot, query)?;
        Ok(Response::Query(QueryInfo::of(
            self.query(&snapshot, id)?,
            budget,
        )))
    }

    /// Block until the query stops being in flight.
    ///
    /// `timeout_ms` is the caller's and the server's is the ceiling: a client
    /// that could ask for an hour would be one that could hold a connection on
    /// the far side of a socket for an hour.
    ///
    /// A wait that runs out answers with the query as it stands rather than
    /// with a timeout failure. Nothing went wrong — the query is still
    /// running, which is what the answer says, and asking again is the caller's
    /// to decide.
    async fn query_wait(
        &self,
        query: &str,
        timeout_ms: Option<u64>,
        budget: Budget,
    ) -> Result<Response, Failure> {
        let snapshot = self.store.snapshot();
        let id = self.query_id(&snapshot, query)?;
        let wanted = timeout_ms.map_or(self.timeout, Duration::from_millis);
        let waited = self
            .store
            .settle(wanted.min(self.timeout), move |s| {
                s.query(id).is_some_and(QueryView::is_settled)
            })
            .await;
        let snapshot = match waited {
            Ok(settled) => settled,
            Err(WaitError::TimedOut) => self.store.snapshot(),
            Err(other) => return Err(self.waited(other)),
        };
        Ok(Response::Query(QueryInfo::of(
            self.query(&snapshot, id)?,
            budget,
        )))
    }

    /// Stop a query, and answer with what it became.
    ///
    /// The busy row is what carries the cancellation, so a query that has
    /// already finished has nothing to cancel — and is answered as it stands
    /// rather than refused, because "it is already done" is the same news.
    async fn query_cancel(&self, query: &str) -> Result<Response, Failure> {
        let snapshot = self.store.snapshot();
        let id = self.query_id(&snapshot, query)?;
        let busy = snapshot
            .busy
            .iter()
            .find(|b| b.owner == BusyOwner::Query(id))
            .map(|b| b.id);

        let snapshot = match busy {
            Some(busy) => {
                self.dispatch_and_settle(Action::Cancel(busy), move |s| {
                    s.query(id).is_some_and(QueryView::is_settled)
                })
                .await?
            }
            None => snapshot,
        };
        Ok(Response::Query(self.query_info(&snapshot, id)?))
    }

    fn query_id(&self, snapshot: &Snapshot, query: &str) -> Result<QueryId, Failure> {
        snapshot
            .queries
            .iter()
            .find(|q| q.id.to_string() == query)
            .map(|q| q.id)
            .ok_or_else(|| Failure::NoSuchQuery {
                query: query.to_owned(),
            })
    }

    fn query<'a>(&self, snapshot: &'a Snapshot, id: QueryId) -> Result<&'a QueryView, Failure> {
        snapshot.query(id).ok_or_else(|| Failure::NoSuchQuery {
            query: id.to_string(),
        })
    }

    fn query_info(&self, snapshot: &Snapshot, id: QueryId) -> Result<QueryInfo, Failure> {
        Ok(QueryInfo::of(self.query(snapshot, id)?, self.budget))
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

    /// Every saved statement, with what each one asks for.
    ///
    /// Read through the store rather than opening the file here, for the same
    /// reason a preview is: a person may have the palette open on the same
    /// list, and two readers of one file is one more than there needs to be.
    async fn templates(&self) -> Result<Response, Failure> {
        let settled = self
            .dispatch_and_settle(Action::LoadTemplates, Snapshot::templates_settled)
            .await?;
        let held = self.listed(&settled)?;
        // The standard's quoting, because a listing is not against a
        // connection: what a template *asks for* does not depend on where it
        // is going, only on how the answers will be quoted when it does.
        let dialect = Dialect {
            quote_style: QuoteStyle::DoubleQuote,
            escaping: Escaping::None,
        };
        Ok(Response::Templates(
            held.iter()
                .map(|template| TemplateInfo::of(template, dialect))
                .collect(),
        ))
    }

    /// Fill one in and answer with the statement.
    async fn apply_template(
        &self,
        name: &str,
        values: &BTreeMap<String, String>,
        connection: Option<&str>,
    ) -> Result<Response, Failure> {
        let settled = self
            .dispatch_and_settle(Action::LoadTemplates, Snapshot::templates_settled)
            .await?;
        let body = self
            .listed(&settled)?
            .iter()
            .find(|template| template.name == name)
            .map(|template| template.body.clone())
            .ok_or_else(|| Failure::NoSuchTemplate {
                name: name.to_owned(),
            })?;

        let dialect = self.dialect(connection).await?;
        match BoundTemplate::bind(&body, values, dialect) {
            Ok(bound) => Ok(Response::Statement(StatementInfo {
                template: name.to_owned(),
                sql: bound.text().to_owned(),
            })),
            // The caller's arguments, and every one of these says which one to
            // fix: a value missing, a name that is not in the body, a body
            // that cannot be read at all.
            Err(why) => Err(Failure::Template {
                message: why.to_string(),
            }),
        }
    }

    fn listed<'a>(&self, settled: &'a Snapshot) -> Result<&'a [Template], Failure> {
        match &settled.templates.data {
            LoadState::Ready(held) => Ok(held.as_slice()),
            // Including "this session is not keeping anything", which is the
            // honest answer to both of these and not an empty list.
            LoadState::Failed(why) => Err(Failure::Unsupported {
                message: why.clone(),
            }),
            _ => Err(Failure::Timeout {
                waited_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }

    /// Whose quoting rules to fill a template in with.
    ///
    /// The named connection, the first ready one, or — with none open — the
    /// standard's. Refusing to fill in a template because nothing is connected
    /// would make quoting a reason somebody cannot write a statement.
    ///
    /// The *first* is a guess, and it is the caller's to correct: two
    /// connections to different drivers quote differently, which is what the
    /// `connection` field is for.
    async fn dialect(&self, connection: Option<&str>) -> Result<Dialect, Failure> {
        let standard = Dialect {
            quote_style: QuoteStyle::DoubleQuote,
            escaping: Escaping::None,
        };
        let Some(named) = connection else {
            return Ok(self
                .store
                .snapshot()
                .connections
                .iter()
                .find(|c| c.is_ready())
                .and_then(|c| c.capabilities.as_ref().map(Dialect::from))
                .unwrap_or(standard));
        };
        let conn = self.connection(named).await?;
        Ok(self
            .store
            .snapshot()
            .connection(conn)
            .and_then(|c| c.capabilities.as_ref().map(Dialect::from))
            .unwrap_or(standard))
    }

    async fn describe(
        &self,
        connection: &str,
        path: &[String],
        refresh: bool,
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
                Action::DescribeTable {
                    conn,
                    table: table.clone(),
                    refresh,
                },
                |s| s.definition_settled(conn, &table),
            )
            .await?;

        match settled.definition(conn, &table).map(|d| &d.data) {
            Some(LoadState::Ready(detail)) => {
                Ok(Response::Definition(DefinitionInfo::of(detail, budget)))
            }
            Some(LoadState::Failed(why)) => Err(Failure::Driver {
                message: why.clone(),
            }),
            // Settled but neither: a person sharing the session closed the tab
            // this was held for, which is the same answer as never having been
            // described.
            _ => Err(Failure::NotFound {
                path: path.to_vec(),
            }),
        }
    }

    /// Sorting is a second action against a preview that already exists.
    ///
    /// `SortPreview` restarts the relation at page one with the ordering
    /// applied — it fetches, rather than reordering the rows already loaded —
    /// so it needs a preview to attach the ordering to, and the wait afterwards
    /// is for a page rather than for a re-render.
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
    use std::time::Duration;

    use serde_json::Value as Json;
    use sqlake_app::store::{Drivers, Wiring};
    use sqlake_core::capability::Capabilities;
    use sqlake_core::id::ProfileId;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::protocol::{FailureKind, ResponseKind, SortBy};
    use crate::snapshot::{EstimateInfo, QueryState, Status};

    async fn service(behaviour: Behaviour) -> (Service, String) {
        service_of(MockDriver::new(behaviour)).await
    }

    /// A service whose session keeps these templates.
    async fn service_keeping(templates: &[(&str, &str)]) -> (Service, String) {
        use sqlake_core::library::Library as _;

        let library = sqlake_library::Sqlite::in_memory().expect("a library opens");
        for (name, body) in templates {
            library
                .add(sqlake_core::library::NewTemplate {
                    name: (*name).to_owned(),
                    body: (*body).to_owned(),
                    driver: None,
                    tags: Vec::new(),
                })
                .expect("it saves");
        }
        let store = Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
                Arc::new(MockProfiles::default()),
            )
            .library(Arc::new(library)),
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

    async fn service_of(driver: MockDriver) -> (Service, String) {
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(driver)),
            Arc::new(MockProfiles::default()),
        ));
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
    async fn opening_a_connection_answers_with_one_that_is_ready() {
        let (service, first) = service(Behaviour::instant()).await;
        let Response::Connection(opened) = service
            .answer(&Request::ConnectionOpen {
                profile: "mock".into(),
            })
            .await
        else {
            panic!("should have opened one");
        };
        // Ready, not connecting: `dispatch` only queues, so an id answered
        // before the store had applied it would be one the next request is
        // told does not exist.
        assert_eq!(opened.status, Status::Ready);
        assert_ne!(opened.id, first, "a second connection, not the first again");

        let Response::Connections(open) = service.answer(&Request::ConnectionList {}).await else {
            panic!("should have listed them");
        };
        assert_eq!(open.len(), 2);
    }

    #[tokio::test]
    async fn a_profile_nobody_configured_is_named_as_a_profile() {
        // Not `NoSuchConnection`: the two are fixed in different files.
        let (service, _) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::ConnectionOpen {
                profile: "nope".into(),
            })
            .await;
        assert!(
            matches!(
                answer,
                Response::Failed(Failure::NoSuchProfile { ref profile }) if profile == "nope"
            ),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn a_connection_that_will_not_open_is_still_a_connection() {
        // It exists, its row says why, and closing it is something a caller
        // may want to do — so the answer is the connection rather than an
        // error about it.
        let (service, _) = service(Behaviour {
            connect_fails: true,
            ..Behaviour::instant()
        })
        .await;
        let Response::Connection(opened) = service
            .answer(&Request::ConnectionOpen {
                profile: "mock".into(),
            })
            .await
        else {
            panic!("should have answered with the connection");
        };
        assert!(matches!(opened.status, Status::Failed { .. }), "{opened:?}");
    }

    #[tokio::test]
    async fn closing_says_what_the_connection_became() {
        let (service, conn) = service(Behaviour::instant()).await;
        let close = Request::ConnectionClose {
            connection: conn.clone(),
        };

        let Response::Connection(gone) = service.answer(&close).await else {
            panic!("should have closed it");
        };
        assert_eq!(gone.status, Status::Closed);

        // Twice is a caller repeating itself, and the honest answer is the
        // connection, closed — not a failure about something that is there.
        let Response::Connection(again) = service.answer(&close).await else {
            panic!("closing twice should answer the same way");
        };
        assert_eq!(again.status, Status::Closed);
    }

    #[tokio::test]
    async fn closing_something_that_was_never_open_is_a_failure() {
        let (service, _) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::ConnectionClose {
                connection: "not-an-id".into(),
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::NoSuchConnection { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn a_closed_connection_takes_its_tree_with_it() {
        // What a caller has to be able to see: reading through a connection
        // after closing it is a failure rather than an empty database.
        let (service, conn) = service(Behaviour::instant()).await;
        let _ = service
            .answer(&Request::ConnectionClose {
                connection: conn.clone(),
            })
            .await;
        let answer = service
            .answer(&Request::NamespaceList {
                connection: conn.clone(),
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Driver { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn a_query_runs_and_its_rows_come_back_on_the_wire() {
        let (service, conn) = service(Behaviour::instant()).await;
        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn,
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };

        let Response::Query(done) = service
            .answer(&Request::QueryWait {
                query: started.id.clone(),
                timeout_ms: None,
                // The limit is on the request that reads the rows, not on
                // the one that started the query — that answers a handle.
                limit: Some(3),
            })
            .await
        else {
            panic!("should have waited");
        };
        assert_eq!(done.id, started.id, "the same query, not a new one");
        let QueryState::Ready { page } = done.state else {
            panic!("{done:?}");
        };
        assert_eq!(page.returned, 3, "the caller asked for three");
        assert!(page.truncated, "and has to be told it did not get them all");
    }

    #[tokio::test]
    async fn estimating_costs_a_statement_without_running_it() {
        let (service, conn) = service_of(
            MockDriver::new(Behaviour {
                estimate_bytes: 4096,
                ..Behaviour::instant()
            })
            .with_capabilities(sqlake_driver_mock::ESTIMATES),
        )
        .await;
        let Response::Query(answer) = service
            .answer(&Request::QueryEstimate {
                connection: conn,
                sql: "select * from public.users".into(),
            })
            .await
        else {
            panic!("should have estimated");
        };
        assert_eq!(answer.estimate, Some(EstimateInfo::Bytes { bytes: 4096 }));
        // Estimated, not ready: no rows were asked for, and saying otherwise
        // would be this surface running something nobody requested.
        assert!(matches!(answer.state, QueryState::Estimated), "{answer:?}");

        // And it leaves nothing behind. A session accumulating a `QueryView`
        // per question an agent asked is a leak with a plausible cause.
        assert!(service.store().snapshot().queries.is_empty());
    }

    #[tokio::test]
    async fn an_estimate_that_outlives_the_wait_leaves_nothing_behind_either() {
        // The case the `?` above used to skip: a timed-out estimate is exactly
        // the one that would strand a `QueryView`, and its busy row with it.
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour {
                latency: Duration::from_millis(300),
                ..Behaviour::instant()
            }))),
            Arc::new(MockProfiles::default()),
        ));
        let conn = ConnId::new();
        // Not through the service: connecting is slow here too, and the point
        // is a short wait on the estimate alone.
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
            .expect("it connects");
        let service = Service::new(store).with_timeout(Duration::from_millis(50));

        let answer = service
            .answer(&Request::QueryEstimate {
                connection: conn.to_string(),
                sql: "select * from public.users".into(),
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Timeout { .. })),
            "{answer:?}"
        );
        let snapshot = service.store().snapshot();
        assert!(snapshot.queries.is_empty(), "the query was forgotten");
        assert!(snapshot.busy.is_empty(), "and so was the work it was doing");
    }

    #[tokio::test]
    async fn a_query_over_the_agents_ceiling_stops_and_says_what_it_would_cost() {
        let (service, conn) = service_of(
            MockDriver::new(Behaviour {
                estimate_bytes: 5_000,
                ..Behaviour::instant()
            })
            .with_capabilities(sqlake_driver_mock::ESTIMATES),
        )
        .await;
        let service = service.with_max_bytes(Some(1_000));

        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn,
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };
        let Response::Query(done) = service
            .answer(&Request::QueryWait {
                query: started.id,
                timeout_ms: None,
                limit: None,
            })
            .await
        else {
            panic!("should have waited");
        };

        // Not a failure: over the budget is an answer, and the number goes to
        // a person this caller cannot be.
        let QueryState::NeedsApproval { budget } = done.state else {
            panic!("{done:?}");
        };
        assert_eq!(budget, 1_000);
        assert_eq!(done.estimate, Some(EstimateInfo::Bytes { bytes: 5_000 }));
    }

    #[tokio::test]
    async fn a_caller_can_lower_the_ceiling_and_never_raise_it() {
        let with_budget = |ours: Option<u64>| {
            Service::new(Store::spawn(Wiring::new(
                Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
                Arc::new(MockProfiles::default()),
            )))
            .with_max_bytes(ours)
        };

        assert_eq!(with_budget(Some(1_000)).max_bytes(Some(500)), Some(500));
        assert_eq!(
            with_budget(Some(1_000)).max_bytes(Some(9_000)),
            Some(1_000),
            "a request cannot raise the surface's own"
        );
        assert_eq!(with_budget(None).max_bytes(Some(500)), Some(500));
        assert_eq!(with_budget(Some(1_000)).max_bytes(None), Some(1_000));
    }

    #[tokio::test]
    async fn a_query_can_be_cancelled_while_it_runs() {
        let (service, conn) = service(Behaviour {
            query_latency: Duration::from_secs(30),
            ..Behaviour::instant()
        })
        .await;
        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn,
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };
        assert!(matches!(started.state, QueryState::Working), "{started:?}");

        let Response::Query(stopped) = service
            .answer(&Request::QueryCancel {
                query: started.id.clone(),
            })
            .await
        else {
            panic!("should have cancelled it");
        };
        let QueryState::Failed { message, .. } = stopped.state else {
            panic!("{stopped:?}");
        };
        assert_eq!(message, "cancelled");
    }

    #[tokio::test]
    async fn cancelling_something_already_finished_says_what_it_became() {
        // "It is already done" is the same news as "it stopped", and refusing
        // would make a caller that raced the query handle an error for
        // something that went right.
        let (service, conn) = service(Behaviour::instant()).await;
        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn,
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };
        let _ = service
            .answer(&Request::QueryWait {
                query: started.id.clone(),
                timeout_ms: None,
                limit: None,
            })
            .await;
        let Response::Query(after) = service
            .answer(&Request::QueryCancel { query: started.id })
            .await
        else {
            panic!("should have answered");
        };
        assert!(matches!(after.state, QueryState::Ready { .. }), "{after:?}");
    }

    #[tokio::test]
    async fn waiting_that_runs_out_answers_with_the_query_rather_than_a_timeout() {
        // Nothing went wrong: the query is still running, which is what the
        // answer says, and asking again is the caller's to decide.
        let (service, conn) = service(Behaviour {
            query_latency: Duration::from_secs(30),
            ..Behaviour::instant()
        })
        .await;
        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn,
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };
        let Response::Query(still) = service
            .answer(&Request::QueryWait {
                query: started.id,
                timeout_ms: Some(50),
                limit: None,
            })
            .await
        else {
            panic!("should have answered");
        };
        assert!(matches!(still.state, QueryState::Working), "{still:?}");
    }

    #[tokio::test]
    async fn a_query_this_session_never_started_is_named_as_a_query() {
        let (service, _) = service(Behaviour::instant()).await;
        for request in [
            Request::QueryStatus {
                query: "nope".into(),
                limit: None,
            },
            Request::QueryWait {
                query: "nope".into(),
                timeout_ms: None,
                limit: None,
            },
            Request::QueryCancel {
                query: "nope".into(),
            },
        ] {
            let answer = service.answer(&request).await;
            assert!(
                matches!(answer, Response::Failed(Failure::NoSuchQuery { .. })),
                "{request:?} answered {answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_write_over_the_socket_is_refused_on_a_read_only_connection() {
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::read_only()),
        ));
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
            .expect("it connects");
        let service = Service::new(store);

        let Response::Query(started) = service
            .answer(&Request::QueryRun {
                connection: conn.to_string(),
                sql: "delete from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started one");
        };
        let Response::Query(done) = service
            .answer(&Request::QueryWait {
                query: started.id,
                timeout_ms: None,
                limit: None,
            })
            .await
        else {
            panic!("should have waited");
        };
        let QueryState::Failed { message, .. } = done.state else {
            panic!("{done:?}");
        };
        assert!(message.contains("read-only"), "{message}");
    }

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
                    connection: conn.clone(),
                    table: vec!["public".into(), "users".into()],
                    sort: None,
                    limit: None,
                })
                .await,
        );

        // A template and a statement built from one, which need a session
        // that keeps something.
        let (keeping, _) = service_keeping(&[("daily", "select * from {{ident:table}}")]).await;
        responses.push(keeping.answer(&Request::TemplateList {}).await);
        responses.push(
            keeping
                .answer(&Request::TemplateApply {
                    template: "daily".into(),
                    values: [("table".to_owned(), "users".to_owned())]
                        .into_iter()
                        .collect(),
                    connection: None,
                })
                .await,
        );

        // A definition, under the narrow budget so that `omitted_columns` is
        // written here too — and against a driver that has indexes, because a
        // `sections` list is empty on the default mock and an empty one is
        // skipped.
        let (described, described_conn) = service_of(
            MockDriver::new(Behaviour::instant()).with_capabilities(Capabilities {
                indexes: true,
                ..sqlake_driver_mock::CAPABILITIES
            }),
        )
        .await;
        responses.push(
            Service::new(described.store().clone())
                .with_budget(narrow)
                .answer(&Request::TableDescribe {
                    connection: described_conn,
                    table: vec!["public".into(), "users".into()],
                    refresh: false,
                })
                .await,
        );
        // And one built by hand for `generated_ddl`, which no driver the tests
        // can reach fills in: the mock has no catalogue to build a statement
        // from, and a definition whose DDL is never serialised is a field this
        // check has not looked at.
        let mut hand_built = sqlake_core::detail::TableDetail::new(
            sqlake_core::node::TableRef::new(["public", "users"]),
            sqlake_core::node::RelationKind::Table,
            Vec::new(),
        );
        hand_built.ddl = Some(sqlake_core::detail::Ddl::generated("CREATE TABLE users ()"));
        responses.push(Response::Definition(DefinitionInfo::of(
            &hand_built,
            Budget::DEFAULT,
        )));

        // Opening, which is one of the two ways to reach `Connection`; closing
        // answers with the same shape.
        responses.push(
            session
                .answer(&Request::ConnectionOpen {
                    profile: "mock".into(),
                })
                .await,
        );

        // A query, run and waited on, which is the only way to reach `Query`
        // with rows on it.
        let Response::Query(started) = session
            .answer(&Request::QueryRun {
                connection: conn.clone(),
                sql: "select * from public.users".into(),
                max_bytes: None,
            })
            .await
        else {
            panic!("should have started a query");
        };
        responses.push(
            session
                .answer(&Request::QueryWait {
                    query: started.id,
                    timeout_ms: None,
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
            Failure::NoSuchProfile {
                profile: "prod".into(),
            },
            Failure::NoSuchQuery { query: "q".into() },
            Failure::NoSuchTemplate {
                name: "daily".into(),
            },
            Failure::Template {
                message: "`table` has no value".into(),
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
                // The same, one level down: a section's rows are catalogue
                // values — an index's own definition, a partitioning
                // expression — and none of them is protocol structure.
                Response::Definition(definition) => {
                    for section in &mut definition.sections {
                        section.page.rows.clear();
                    }
                }
                _ => {}
            }
            keys(
                &serde_json::to_value(&*response).expect("a response serialises"),
                &mut found,
            );
        }

        let expected: BTreeSet<String> = [
            "body",
            "cancel",
            "capabilities",
            "columns",
            "comment",
            "connection",
            "connections",
            "cost_estimate",
            "data",
            "default",
            "driver",
            "error",
            // A query's own vocabulary. `sql` is the statement the caller
            // sent back to it, and `measured` says which unit an estimate is
            // in — neither is derived from a credential, which is what this
            // list is watching for.
            "estimate",
            "free_preview",
            // A definition's own vocabulary. `generated_ddl` is a statement
            // this client built from the catalogue, and `title` is the
            // driver's name for a section — a server's words, not a
            // credential's.
            "generated_ddl",
            "hierarchy",
            "id",
            "kind",
            "loaded",
            "message",
            "name",
            "nullable",
            "measured",
            "omitted_columns",
            "page",
            "path",
            "profile",
            "profiles",
            "query",
            "reason",
            "relation_kind",
            "response",
            "returned",
            "rows",
            "sections",
            "sortable_preview",
            "placeholders",
            "sql",
            "state",
            "stats",
            "status",
            "table",
            "template",
            "title",
            "total",
            "truncated",
            "type_name",
            "value",
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

    fn definition(response: &Response) -> &DefinitionInfo {
        match response {
            Response::Definition(definition) => definition,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_definition_answers_what_a_relation_is_rather_than_what_is_in_it() {
        let (service, conn) = service_of(MockDriver::new(Behaviour::instant()).with_capabilities(
            Capabilities {
                indexes: true,
                ..sqlake_driver_mock::CAPABILITIES
            },
        ))
        .await;
        let answer = service
            .answer(&Request::TableDescribe {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                refresh: false,
            })
            .await;

        let definition = definition(&answer);
        assert_eq!(definition.table, ["public", "users"]);
        assert_eq!(definition.relation_kind, "table");
        // The point of the whole layer: an agent branching on nullability
        // reads a boolean, where the TUI reads the words "not null".
        let first = definition.columns.first().expect("a column");
        assert!(!first.nullable);
        assert!(first.default.is_some(), "{first:?}");
        assert_eq!(
            definition
                .sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Indexes"]
        );
        assert!(!definition.stats.is_empty(), "{definition:?}");
    }

    #[tokio::test]
    async fn a_template_is_listed_with_what_it_asks_for() {
        // So a caller need not find `{{…}}` in the body itself: a second
        // parser is a second answer to what a template needs.
        let (service, _) =
            service_keeping(&[("daily", "select * from {{ident:table}} where d = {{day}}")]).await;
        let answer = service.answer(&Request::TemplateList {}).await;
        let Response::Templates(held) = &answer else {
            panic!("{answer:?}");
        };
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].name, "daily");
        assert_eq!(
            held[0]
                .placeholders
                .iter()
                .map(|p| (p.name.as_str(), p.kind.as_str()))
                .collect::<Vec<_>>(),
            [("table", "ident"), ("day", "value")]
        );
        assert_eq!(held[0].unreadable, None);
    }

    #[tokio::test]
    async fn a_template_that_cannot_be_read_is_still_listed_and_says_why() {
        // Dropping it would leave a saved statement that is invisible; saying
        // it has no placeholders would send a caller on to apply it and be
        // refused for a reason it could have had here.
        let (service, _) = service_keeping(&[("broken", "select '{{x}}'")]).await;
        let answer = service.answer(&Request::TemplateList {}).await;
        let Response::Templates(held) = &answer else {
            panic!("{answer:?}");
        };
        assert!(held[0].placeholders.is_empty());
        assert!(held[0].unreadable.is_some(), "{held:?}");
    }

    #[tokio::test]
    async fn applying_a_template_answers_sql_and_runs_nothing() {
        let (service, _) =
            service_keeping(&[("daily", "select * from {{ident:table}} where n = {{name}}")]).await;
        let answer = service
            .answer(&Request::TemplateApply {
                template: "daily".into(),
                values: [
                    ("table".to_owned(), "users".to_owned()),
                    ("name".to_owned(), "o'brien".to_owned()),
                ]
                .into_iter()
                .collect(),
                connection: None,
            })
            .await;
        let Response::Statement(statement) = &answer else {
            panic!("{answer:?}");
        };
        assert_eq!(
            statement.sql,
            r#"select * from "users" where n = 'o''brien'"#
        );
        assert_eq!(statement.template, "daily");
        // And nothing ran: no query exists to have run.
        assert!(service.store().snapshot().queries.is_empty());
    }

    #[tokio::test]
    async fn a_value_left_out_says_which_one() {
        let (service, _) = service_keeping(&[("daily", "select {{a}}, {{b}}")]).await;
        let answer = service
            .answer(&Request::TemplateApply {
                template: "daily".into(),
                values: [("a".to_owned(), "1".to_owned())].into_iter().collect(),
                connection: None,
            })
            .await;
        let Response::Failed(Failure::Template { message }) = &answer else {
            panic!("{answer:?}");
        };
        assert!(message.contains('b'), "{message}");
    }

    #[tokio::test]
    async fn a_template_nobody_saved_is_named_as_a_template() {
        // Rather than as a path that was not found: a caller that used the
        // wrong name needs to be told which kind of thing it got wrong.
        let (service, _) = service_keeping(&[]).await;
        let answer = service
            .answer(&Request::TemplateApply {
                template: "nope".into(),
                values: std::collections::BTreeMap::new(),
                connection: None,
            })
            .await;
        assert_eq!(
            answer,
            Response::Failed(Failure::NoSuchTemplate {
                name: "nope".into()
            })
        );
    }

    #[tokio::test]
    async fn a_session_keeping_nothing_says_so_rather_than_listing_none() {
        // An empty list would say "you have no saved statements", which is a
        // different thing from "this session cannot keep any".
        let (service, _) = service(Behaviour::instant()).await;
        let answer = service.answer(&Request::TemplateList {}).await;
        let Response::Failed(Failure::Unsupported { message }) = &answer else {
            panic!("{answer:?}");
        };
        assert!(message.contains("keeping"), "{message}");
    }

    #[tokio::test]
    async fn a_definition_needs_no_listing_first() {
        // The same promise `a_preview_needs_no_listing_first` makes: a caller
        // names a path, it does not replay the clicks that would bring the
        // path onto a screen.
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TableDescribe {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                refresh: false,
            })
            .await;
        assert!(matches!(answer, Response::Definition(_)), "{answer:?}");
    }

    #[tokio::test]
    async fn describing_something_that_is_not_a_relation_says_so() {
        let (service, conn) = service(Behaviour::instant()).await;
        let answer = service
            .answer(&Request::TableDescribe {
                connection: conn,
                table: vec!["public".into()],
                refresh: false,
            })
            .await;
        assert!(
            matches!(answer, Response::Failed(Failure::Unsupported { .. })),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn only_a_refresh_asks_the_driver_again() {
        // Succeeds once and fails afterwards, so what the driver was asked is
        // readable from the answers: a second call that comes back fine did
        // not reach it, and one that fails did.
        let table = vec!["public".to_owned(), "users".to_owned()];
        let (service, conn) = service(Behaviour {
            failing_after: vec![(table.clone(), 1)],
            ..Behaviour::instant()
        })
        .await;
        let describe = |refresh| Request::TableDescribe {
            connection: conn.clone(),
            table: table.clone(),
            refresh,
        };

        assert!(
            matches!(
                service.answer(&describe(false)).await,
                Response::Definition(_)
            ),
            "the first describe should have been answered"
        );
        assert!(
            matches!(
                service.answer(&describe(false)).await,
                Response::Definition(_)
            ),
            "a second describe read what the session already held"
        );
        assert!(
            matches!(
                service.answer(&describe(true)).await,
                Response::Failed(Failure::Driver { .. })
            ),
            "a refresh should have gone back to the driver"
        );
    }

    #[tokio::test]
    async fn a_driver_that_will_not_describe_says_why() {
        let (service, conn) = service(Behaviour {
            failing_nodes: vec![vec!["public".to_owned(), "users".to_owned()]],
            ..Behaviour::instant()
        })
        .await;
        let answer = service
            .answer(&Request::TableDescribe {
                connection: conn,
                table: vec!["public".into(), "users".into()],
                refresh: false,
            })
            .await;
        let Response::Failed(Failure::Driver { message }) = &answer else {
            panic!("{answer:?}");
        };
        assert!(message.contains("permission denied"), "{message}");
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
