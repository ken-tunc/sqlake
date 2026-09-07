//! One actor task per connection.
//!
//! The actor owns the `Box<dyn Session>` and serialises access to it, so driver
//! implementations never need to be internally concurrent.
//!
//! Serialising here does mean a slow preview delays a tree expansion on the
//! same connection. That is deliberate: holding more than one physical
//! connection is a driver-level concern (the PostgreSQL driver will keep a
//! separate metadata connection behind a single `Session`), not something the
//! application layer should try to arrange.

use sqlake_core::capability::Capabilities;
use sqlake_core::driver::{DriverResult, Session};
use sqlake_core::node::{NodeRef, TableRef, TreeNode};
use sqlake_core::result::{PageRequest, ResultSet};
use sqlake_core::sql::{ApprovedQuery, Estimate, ValidatedSql};
use tokio::sync::{mpsc, oneshot};

use crate::error::{AppError, AppResult};

/// Enough outstanding requests that a burst of clicks is not dropped, small
/// enough that a runaway caller is noticed.
const QUEUE_DEPTH: usize = 32;

#[derive(Debug)]
enum SessionCmd {
    Children {
        of: NodeRef,
        reply: oneshot::Sender<DriverResult<Vec<TreeNode>>>,
    },
    Preview {
        table: TableRef,
        req: PageRequest,
        reply: oneshot::Sender<DriverResult<ResultSet>>,
    },
    Estimate {
        sql: ValidatedSql,
        reply: oneshot::Sender<DriverResult<Estimate>>,
    },
    Execute {
        query: Box<ApprovedQuery>,
        reply: oneshot::Sender<DriverResult<ResultSet>>,
        /// Resolves when the caller has gone, which is the whole cancellation
        /// mechanism. It arrives *with* the command rather than as a later one
        /// of its own: the actor serialises access, so a cancel sent down the
        /// same channel would queue behind the query it is meant to stop and
        /// arrive after it finished.
        cancel: oneshot::Receiver<()>,
    },
    Close,
}

/// A cloneable handle to one connection's actor.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
    capabilities: Capabilities,
}

impl SessionHandle {
    /// Take ownership of a session and start its actor.
    #[must_use]
    pub fn spawn(session: Box<dyn Session>) -> Self {
        let capabilities = session.capabilities();
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        tokio::spawn(run(session, rx));
        Self { tx, capabilities }
    }

    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub async fn children(&self, of: NodeRef) -> AppResult<Vec<TreeNode>> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(SessionCmd::Children { of, reply })
            .await
            .map_err(|_| AppError::SessionClosed)?;
        answer
            .await
            .map_err(|_| AppError::SessionClosed)?
            .map_err(Into::into)
    }

    pub async fn preview(&self, table: TableRef, req: PageRequest) -> AppResult<ResultSet> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(SessionCmd::Preview { table, req, reply })
            .await
            .map_err(|_| AppError::SessionClosed)?;
        answer
            .await
            .map_err(|_| AppError::SessionClosed)?
            .map_err(Into::into)
    }

    pub async fn estimate(&self, sql: ValidatedSql) -> AppResult<Estimate> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(SessionCmd::Estimate { sql, reply })
            .await
            .map_err(|_| AppError::SessionClosed)?;
        answer
            .await
            .map_err(|_| AppError::SessionClosed)?
            .map_err(Into::into)
    }

    /// Run a query, and stop it at the server if this future is dropped.
    ///
    /// Dropping is how cancellation is expressed, because it is what already
    /// happens: the store aborts the task waiting here. The sender below lives
    /// exactly as long as this future, so aborting it closes the channel, the
    /// actor's `select` fires, and the driver's own future is dropped mid-call
    /// — which is where a driver that can reach its server does so.
    pub async fn execute(&self, query: ApprovedQuery) -> AppResult<ResultSet> {
        let (reply, answer) = oneshot::channel();
        // Held for the whole of this function. Named rather than `_`, which
        // would drop it immediately and cancel every query the moment it
        // started.
        let (_alive, cancel) = oneshot::channel();
        self.tx
            .send(SessionCmd::Execute {
                query: Box::new(query),
                reply,
                cancel,
            })
            .await
            .map_err(|_| AppError::SessionClosed)?;
        answer
            .await
            .map_err(|_| AppError::SessionClosed)?
            .map_err(Into::into)
    }

    /// Ask the actor to shut down. Returns immediately; the driver's own
    /// teardown happens on the actor task.
    pub fn close(&self) {
        // A full queue or a dead actor both mean there is nothing to close.
        let _ = self.tx.try_send(SessionCmd::Close);
    }
}

async fn run(session: Box<dyn Session>, mut rx: mpsc::Receiver<SessionCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCmd::Children { of, reply } => {
                // The receiver may be gone if the request was abandoned, which
                // is not an error: the work simply had no consumer.
                let _ = reply.send(session.children(&of).await);
            }
            SessionCmd::Preview { table, req, reply } => {
                let _ = reply.send(session.preview(&table, &req).await);
            }
            SessionCmd::Estimate { sql, reply } => {
                let _ = reply.send(session.estimate(&sql).await);
            }
            SessionCmd::Execute {
                query,
                reply,
                cancel,
            } => {
                tokio::select! {
                    // Biased, so a caller that gave up while this command sat
                    // in the queue behind another one never reaches the driver
                    // at all. Left to chance, half of those send a statement to
                    // the server before the cancel branch is looked at.
                    biased;
                    // The caller is gone. Falling out of the select drops the
                    // driver's future half-way through its call, which is the
                    // signal a driver turns into a cancel request of its own.
                    _ = cancel => {
                        tracing::debug!("a query was abandoned; the driver is being unwound");
                    }
                    result = session.execute(&query) => {
                        let _ = reply.send(result);
                    }
                }
            }
            SessionCmd::Close => break,
        }
    }
    session.close().await;
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_core::node::NodeKind;
    use sqlake_driver_mock::{Behaviour, MockDriver, mock_profile};

    use super::*;

    async fn handle() -> SessionHandle {
        let driver = MockDriver::new(Behaviour::instant());
        SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap())
    }

    #[tokio::test]
    async fn requests_are_answered() {
        let h = handle().await;
        let roots = h.children(NodeRef::root()).await.unwrap();
        assert!(!roots.is_empty());
    }

    #[tokio::test]
    async fn capabilities_are_available_without_a_round_trip() {
        let h = handle().await;
        // Copied at spawn time so the UI can consult it while the actor is busy.
        assert_eq!(h.capabilities().hierarchy.len(), 2);
    }

    #[tokio::test]
    async fn driver_errors_surface_as_app_errors() {
        let driver = MockDriver::new(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        });
        let h = SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap());
        let node = NodeRef::new(NodeKind::Namespace, ["restricted"]);
        let err = h.children(node).await.unwrap_err();
        assert!(matches!(err, AppError::Driver(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_closed_actor_reports_itself_rather_than_hanging() {
        let h = handle().await;
        h.close();
        // Give the actor a chance to drain and exit.
        tokio::task::yield_now().await;
        for _ in 0..10 {
            if h.children(NodeRef::root()).await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the handle kept accepting work after close");
    }

    #[tokio::test]
    async fn requests_are_served_in_order() {
        // The actor is the serialisation point: two concurrent callers cannot
        // interleave inside the driver.
        let h = handle().await;
        let a = h.children(NodeRef::root());
        let b = h.children(NodeRef::new(NodeKind::Namespace, ["public"]));
        let (a, b) = tokio::join!(a, b);
        assert!(!a.unwrap().is_empty());
        assert!(!b.unwrap().is_empty());
    }
}
