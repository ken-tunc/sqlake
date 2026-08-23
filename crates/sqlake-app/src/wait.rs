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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlake_core::id::{ConnId, ProfileId};
    use sqlake_core::node::{NodeKind, NodeRef, TableRef};
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::action::Action;
    use crate::store::Drivers;

    const LIMIT: Duration = Duration::from_secs(5);

    fn store_of(behaviour: Behaviour) -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
        )
    }

    fn mock() -> ProfileId {
        ProfileId::parse("mock").expect("a usable id")
    }

    /// T2's done-when: the three things the read-only surface does, driven to
    /// completion with no terminal and no loop of the caller's own.
    #[tokio::test]
    async fn connect_expand_preview_runs_headless() {
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);
        let users = TableRef::new(["public", "users"]);

        store.dispatch(Action::Connect {
            profile: mock(),
            conn,
        });
        store
            .settle(LIMIT, |s| s.connection_settled(conn))
            .await
            .expect("the connection settles");

        store.dispatch(Action::ExpandNode {
            conn,
            node: public.clone(),
        });
        let snap = store
            .settle(LIMIT, |s| s.node_settled(conn, &public))
            .await
            .expect("the node settles");
        assert!(snap.tree(conn).any(|n| n.label == "users"));

        store.dispatch(Action::PreviewTable {
            conn,
            table: users.clone(),
        });
        let snap = store
            .settle(LIMIT, |s| s.preview_settled(conn, &users))
            .await
            .expect("the preview settles");
        assert!(snap.preview(conn, &users).unwrap().data.ready().is_some());
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
        store.dispatch(Action::Connect {
            profile: mock(),
            conn,
        });
        store
            .settle(LIMIT, |s| s.connection_settled(conn))
            .await
            .expect("the connection settles");

        store.dispatch(Action::PreviewTable {
            conn,
            table: broken.clone(),
        });
        let snap = store
            .settle(LIMIT, |s| s.preview_settled(conn, &broken))
            .await
            .expect("a failed page is settled");
        assert!(snap.preview(conn, &broken).unwrap().data.error().is_some());

        let never = store
            .settle(Duration::from_millis(50), |s| {
                s.preview(conn, &broken)
                    .is_some_and(|p| p.data.ready().is_some())
            })
            .await;
        assert_eq!(never.unwrap_err(), WaitError::TimedOut);
    }

    #[tokio::test]
    async fn a_condition_that_already_holds_needs_no_new_snapshot() {
        // Otherwise every wait costs one publication, and a caller asking
        // about work that finished before it looked would hang until the
        // store happened to change for some other reason.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        store.dispatch(Action::Connect {
            profile: mock(),
            conn,
        });
        store
            .settle(LIMIT, |s| s.connection_settled(conn))
            .await
            .expect("the connection settles");

        assert!(
            store
                .settle(Duration::from_millis(50), |s| s.connection_settled(conn))
                .await
                .is_ok()
        );
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
        // refused action publishes nothing, so it is indistinguishable from
        // one that is slow. A caller that can see the snapshot should look
        // before it dispatches; this is what it gets if it does not.
        let store = store_of(Behaviour::instant());
        let conn = ConnId::new();
        store.dispatch(Action::Connect {
            profile: ProfileId::parse("typo").expect("a usable id"),
            conn,
        });
        assert_eq!(
            store
                .settle(Duration::from_millis(50), |s| s.connection_settled(conn))
                .await
                .unwrap_err(),
            WaitError::TimedOut
        );
    }
}
