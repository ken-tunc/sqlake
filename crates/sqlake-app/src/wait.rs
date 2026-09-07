//! Waiting for the store to finish something, for a caller that cannot watch.
//!
//! The store publishes state and never replies to whoever dispatched an
//! action, so "finished" has to be a predicate over the snapshot rather than an
//! answer to a request. Giving it a correlation id per action would put a
//! second control path over the same state, and two paths can disagree — the
//! reply says a preview is ready while the snapshot everyone else reads still
//! says loading.
//!
//! The TUI does not need any of this: it redraws on every snapshot, so it is
//! already looking at the answer. A request/response caller has to know when to
//! print.

use std::sync::Arc;
use std::time::Duration;

use crate::action::Action;
use crate::snapshot::Snapshot;
use crate::store::Store;

/// Why a wait ended without its condition holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    #[error("timed out")]
    TimedOut,
    /// The store stopped publishing, so the condition can never hold now.
    /// Reported separately because waiting again would be pointless rather
    /// than merely slow.
    #[error("the store has stopped")]
    Stopped,
}

impl Store {
    /// Wait until `done` holds of a published snapshot, and return that
    /// snapshot.
    ///
    /// `done` must accept failure as an ending. A predicate that only holds on
    /// success — "this preview has rows" — waits out the full timeout on a page
    /// that failed, and reports a timeout for something that finished. The
    /// `*_settled` predicates on [`Snapshot`] are there to be used instead of
    /// hand-rolling that.
    ///
    /// In a session someone is also clicking in, the condition can be satisfied
    /// by work their clicks started rather than by the action this caller
    /// dispatched. For the read-only, idempotent requests the agent surface
    /// makes, that is the right answer: "this node is loaded" is what was
    /// asked for, whoever loaded it.
    ///
    /// The first snapshot examined is the one already published, so this waits
    /// for a condition whoever caused it — it cannot tell state left by an
    /// earlier request from state left by this caller's. To wait for the result
    /// of a particular action, dispatch it with
    /// [`Store::dispatch_and_settle`].
    pub async fn settle(
        &self,
        timeout: Duration,
        done: impl Fn(&Snapshot) -> bool,
    ) -> Result<Arc<Snapshot>, WaitError> {
        let mut snapshots = self.subscribe();
        let wait = async {
            loop {
                // Cloned out rather than held across the await: a `watch`
                // borrow blocks the store from publishing, and the store is the
                // only thing that can make this condition true.
                let snapshot = snapshots.borrow_and_update().clone();
                if done(&snapshot) {
                    return Ok(snapshot);
                }
                if snapshots.changed().await.is_err() {
                    return Err(WaitError::Stopped);
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or(Err(WaitError::TimedOut))
    }

    /// Dispatch, then wait for the store to finish what it was asked.
    ///
    /// `dispatch` only queues, so a bare [`Store::settle`] starts by examining
    /// state that predates the action — and every state a `*_settled` predicate
    /// could read is reachable from before the request as well as after it. A
    /// preview being retried is `Failed` until the retry lands, so the wait ends
    /// on the failure it was retrying out of and the caller reports it as the
    /// answer to its own request. Appending a page leaves the preview `Ready`
    /// throughout, so the wait ends before the new rows exist.
    ///
    /// Waiting for the *next* snapshot instead would only narrow the window:
    /// any other caller's action publishes one too, and on a shared session
    /// there are other callers. So the store counts what it has applied and
    /// hands the ordinal back here.
    pub async fn dispatch_and_settle(
        &self,
        action: Action,
        timeout: Duration,
        done: impl Fn(&Snapshot) -> bool,
    ) -> Result<Arc<Snapshot>, WaitError> {
        let dispatched = self.dispatch(action);
        self.settle(timeout, |s| s.has_applied(dispatched) && done(s))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlake_core::id::{ConnId, ProfileId};
    use sqlake_core::node::{NodeKind, NodeRef, TableRef};
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::store::Drivers;

    const LIMIT: Duration = Duration::from_secs(5);
    /// Long enough to be sure nothing lands, short enough to be free.
    const BRIEF: Duration = Duration::from_millis(200);

    fn store_of(behaviour: Behaviour) -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
            None,
        )
    }

    fn mock() -> ProfileId {
        ProfileId::parse("mock").expect("a usable id")
    }

    async fn connected(store: &Store, conn: ConnId) {
        store
            .dispatch_and_settle(
                Action::Connect {
                    profile: mock(),
                    conn,
                },
                LIMIT,
                |s| s.connection_settled(conn),
            )
            .await
            .expect("the connection settles");
    }

    /// T2's done-when: the three things the read-only surface does, driven to
    /// completion with no terminal and no loop of the caller's own.
    #[tokio::test]
    async fn connect_expand_preview_runs_headless() {
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);
        let users = TableRef::new(["public", "users"]);
        connected(&store, conn).await;

        let snap = store
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: public.clone(),
                },
                LIMIT,
                |s| s.node_settled(conn, &public),
            )
            .await
            .expect("the node settles");
        assert!(snap.tree(conn).any(|n| n.label == "users"));

        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: users.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &users),
            )
            .await
            .expect("the preview settles");
        assert!(snap.preview(conn, &users).unwrap().data.ready().is_some());
    }

    #[tokio::test]
    async fn a_retry_waits_for_the_retry() {
        // The reason a wait needs a barrier at all. A preview being retried is
        // `Failed` until the new page lands, so a wait that starts by reading
        // the state ends on the failure it is retrying out of — and the caller
        // prints that as the answer to its own request.
        let store = store_of(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let conn = ConnId::new();
        let users = TableRef::new(["public", "users"]);
        connected(&store, conn).await;

        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: users.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &users),
            )
            .await
            .expect("the first attempt settles");
        assert!(snap.preview(conn, &users).unwrap().data.error().is_some());

        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: users.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &users),
            )
            .await
            .expect("the retry settles");
        assert!(
            snap.preview(conn, &users).unwrap().data.ready().is_some(),
            "the wait ended on the attempt it was retrying out of"
        );
    }

    #[tokio::test]
    async fn appending_a_page_waits_for_the_page() {
        // The same hole from the other side: an append leaves the preview
        // `Ready` throughout, so nothing about its state marks the new rows as
        // having arrived.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        let big = TableRef::new(["public", "big"]);
        connected(&store, conn).await;

        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: big.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &big),
            )
            .await
            .expect("the first page settles");
        let first = snap.preview(conn, &big).unwrap().loaded_rows;

        let snap = store
            .dispatch_and_settle(
                Action::LoadMore {
                    conn,
                    table: big.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &big),
            )
            .await
            .expect("the next page settles");
        assert!(
            snap.preview(conn, &big).unwrap().loaded_rows > first,
            "the wait ended before the page it asked for"
        );
    }

    #[tokio::test]
    async fn a_node_under_a_closed_row_still_settles() {
        // A wait must not depend on the tree being drawn. In a session someone
        // else is using, a connection's row can be closed at any moment — and
        // `flatten` emits nothing underneath a closed one, so a wait reading
        // the visible rows reports a hang for children that arrived.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);
        connected(&store, conn).await;

        store
            .dispatch_and_settle(
                Action::ToggleNode {
                    conn,
                    node: NodeRef::root(),
                },
                LIMIT,
                |s| s.tree(conn).count() == 0,
            )
            .await
            .expect("the row closes");

        store
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: public.clone(),
                },
                LIMIT,
                |s| s.node_settled(conn, &public),
            )
            .await
            .expect("a node nobody can see still settles");
    }

    /// Slow enough that a wait which returns early returns before the reply.
    fn slow(paths: Vec<Vec<String>>) -> Behaviour {
        Behaviour {
            slow_nodes: paths,
            slow_latency: Duration::from_millis(150),
            ..Behaviour::instant()
        }
    }

    #[tokio::test]
    async fn a_node_wait_ends_when_the_children_arrive() {
        let store = store_of(slow(vec![vec!["analytics".to_owned()]]));
        let conn = ConnId::new();
        let analytics = NodeRef::new(NodeKind::Namespace, ["analytics"]);
        connected(&store, conn).await;

        let snap = store
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: analytics.clone(),
                },
                LIMIT,
                |s| s.node_settled(conn, &analytics),
            )
            .await
            .expect("the node settles");
        assert!(
            snap.tree(conn).any(|n| n.label == "daily_summary"),
            "the wait ended before the children it asked for"
        );
    }

    #[tokio::test]
    async fn a_wait_is_about_its_own_target() {
        // Otherwise a busy session is one where nothing settles: every wait
        // would be held open by whatever else the store happens to be doing,
        // which on a shared session is somebody else's work entirely.
        //
        // Two connections because one session actor serialises its own
        // requests — a slow preview really does delay everything behind it on
        // the same connection, which `session` documents as deliberate.
        let store = store_of(slow(vec![vec!["analytics".to_owned(), "slow".to_owned()]]));
        let (busy, quick) = (ConnId::new(), ConnId::new());
        let users = TableRef::new(["public", "users"]);
        connected(&store, busy).await;
        connected(&store, quick).await;

        store.dispatch(Action::PreviewTable {
            conn: busy,
            table: TableRef::new(["analytics", "slow"]),
        });
        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn: quick,
                    table: users.clone(),
                },
                Duration::from_millis(100),
                |s| s.preview_settled(quick, &users),
            )
            .await
            .expect("a wait held open by another connection's work");
        assert!(snap.preview(quick, &users).unwrap().data.ready().is_some());
    }

    #[tokio::test]
    async fn a_wait_is_about_its_own_relation() {
        // One connection, so both previews queue at the same session actor and
        // the second cannot finish before the first. A wait that asked about
        // the first must return without waiting for the second — the snapshot
        // it returns cannot be asserted on, because `watch` keeps only the
        // latest value and the store may publish past an intermediate state
        // before this task is polled.
        let store = store_of(Behaviour {
            slow_nodes: vec![vec!["analytics".to_owned(), "slow".to_owned()]],
            slow_latency: Duration::from_secs(5),
            ..Behaviour::instant()
        });
        let conn = ConnId::new();
        let users = TableRef::new(["public", "users"]);
        connected(&store, conn).await;

        store.dispatch(Action::PreviewTable {
            conn,
            table: users.clone(),
        });
        let queued = store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["analytics", "slow"]),
        });
        store
            .settle(Duration::from_secs(1), |s| {
                s.has_applied(queued) && s.preview_settled(conn, &users)
            })
            .await
            .expect("a wait held open by another relation on the same connection");
    }

    #[tokio::test]
    async fn a_failure_is_an_ending() {
        // The trap the `*_settled` predicates exist for: a caller waiting on
        // "this preview has rows" waits out the whole timeout on a page that
        // failed, then reports a timeout for something that finished.
        let store = store_of(Behaviour {
            failing_nodes: vec![vec!["analytics".to_owned(), "broken".to_owned()]],
            ..Behaviour::instant()
        });
        let conn = ConnId::new();
        let broken = TableRef::new(["analytics", "broken"]);
        connected(&store, conn).await;

        let snap = store
            .dispatch_and_settle(
                Action::PreviewTable {
                    conn,
                    table: broken.clone(),
                },
                LIMIT,
                |s| s.preview_settled(conn, &broken),
            )
            .await
            .expect("a failed page is settled");
        assert!(snap.preview(conn, &broken).unwrap().data.error().is_some());

        let never = store
            .settle(BRIEF, |s| {
                s.preview(conn, &broken)
                    .is_some_and(|p| p.data.ready().is_some())
            })
            .await;
        assert_eq!(never.unwrap_err(), WaitError::TimedOut);
    }

    #[tokio::test]
    async fn work_that_finished_before_anyone_looked_still_ends_a_wait() {
        // Otherwise every wait costs a publication, and a caller asking about
        // something already done hangs until the store changes for an unrelated
        // reason.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        connected(&store, conn).await;
        store
            .settle(BRIEF, |s| s.connection_settled(conn))
            .await
            .expect("already settled");
    }

    #[tokio::test]
    async fn waiting_on_a_store_that_has_quit_says_so() {
        // Distinct from a timeout: waiting again would be pointless rather
        // than merely slow, and a caller printing "timed out" for a store that
        // is gone sends whoever reads it looking for a slow database.
        let store = store_of(Behaviour::instant());
        store.dispatch(Action::Quit);
        assert_eq!(
            store.settle(LIMIT, |_| false).await.unwrap_err(),
            WaitError::Stopped
        );
    }

    #[tokio::test]
    async fn an_action_the_store_refuses_times_out() {
        // The cost of "finished" being a predicate rather than a reply: a
        // refused action publishes nothing of its own, so it is
        // indistinguishable from one that is slow. A caller that can see the
        // snapshot should look before it dispatches; this is what it gets if it
        // does not.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        let ghost = NodeRef::new(NodeKind::Namespace, ["ghost"]);
        connected(&store, conn).await;

        let refused = store
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: ghost.clone(),
                },
                BRIEF,
                // Not `node_settled`, which is true of a node nobody asked
                // about: the honest question here is whether it ever loaded.
                |s| s.tree(conn).any(|n| n.node_ref == ghost),
            )
            .await;
        assert_eq!(refused.unwrap_err(), WaitError::TimedOut);
    }
}
