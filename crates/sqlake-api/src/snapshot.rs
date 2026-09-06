//! What a caller is told about the session's state.
//!
//! Nothing here carries a host, a user, or anything derived from a credential.
//! `Snapshot` holds no `ResolvedProfile` today, and serialisation must not be
//! the thing that changes that: a connection is an id, a name, a driver and a
//! status, and the socket is the one place where an accidental field would
//! leave the process.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlake_app::snapshot::{ConnStatus, ConnectionView, Snapshot};
use sqlake_app::tree::{NodeState, VisibleNode};
use sqlake_core::capability::Capabilities;
use sqlake_core::id::ConnId;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Status {
    Connecting,
    Ready,
    Failed { reason: String },
    Closed,
}

impl From<&ConnStatus> for Status {
    fn from(status: &ConnStatus) -> Self {
        match status {
            ConnStatus::Connecting => Self::Connecting,
            ConnStatus::Ready => Self::Ready,
            ConnStatus::Failed(reason) => Self::Failed {
                reason: reason.clone(),
            },
            ConnStatus::Closed => Self::Closed,
        }
    }
}

/// What a front-end is allowed to assume about a connection.
///
/// Only the answers a read-only caller can act on. The hierarchy is here
/// because it is what tells an agent whether a path is `schema.table` or
/// `project.dataset.table` without it having to know which driver it reached.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct CapabilityInfo {
    pub hierarchy: Vec<String>,
    pub sortable_preview: bool,
    pub free_preview: bool,
    pub cost_estimate: bool,
    pub cancel: bool,
}

impl From<&Capabilities> for CapabilityInfo {
    fn from(caps: &Capabilities) -> Self {
        Self {
            hierarchy: caps.hierarchy.iter().map(|l| l.label.to_owned()).collect(),
            sortable_preview: caps.sortable_preview,
            free_preview: caps.free_preview,
            cost_estimate: caps.cost_estimate,
            cancel: caps.cancel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct ConnectionInfo {
    pub id: String,
    /// Which configured profile it came from. A name in a file the user wrote,
    /// not anything the profile resolved to.
    pub profile: String,
    pub name: String,
    pub driver: String,
    #[serde(flatten)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilityInfo>,
}

impl From<&ConnectionView> for ConnectionInfo {
    fn from(conn: &ConnectionView) -> Self {
        Self {
            id: conn.id.to_string(),
            profile: conn.profile.as_str().to_owned(),
            name: conn.name.clone(),
            driver: conn.kind.as_str().to_owned(),
            status: Status::from(&conn.status),
            capabilities: conn.capabilities.as_ref().map(CapabilityInfo::from),
        }
    }
}

/// A profile that could be connected to.
///
/// The colour a profile carries is left out: it is the TUI's way of making a
/// production connection not look like a scratch one, and means nothing to a
/// caller with no screen.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub driver: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Has no children at all.
    Leaf,
    Collapsed,
    Loading,
    Loaded,
    Failed,
}

/// One object in the tree.
///
/// The path is the address every other request uses, so it is given whole
/// rather than as a name that has to be joined back together — a namespace
/// containing a dot is a real thing, and rebuilding `public.my.table` from
/// pieces is where that goes wrong.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct NodeInfo {
    pub path: Vec<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_kind: Option<String>,
    pub status: NodeStatus,
    /// Present only on a failure, and is the driver's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<&VisibleNode> for NodeInfo {
    fn from(node: &VisibleNode) -> Self {
        Self {
            path: node.node_ref.path.clone(),
            name: node.label.clone(),
            relation_kind: node.relation_kind.map(|k| k.as_str().to_owned()),
            status: match &node.state {
                NodeState::Leaf => NodeStatus::Leaf,
                NodeState::Collapsed => NodeStatus::Collapsed,
                NodeState::Loading => NodeStatus::Loading,
                NodeState::Expanded => NodeStatus::Loaded,
                NodeState::Failed(_) => NodeStatus::Failed,
            },
            error: match &node.state {
                NodeState::Failed(why) => Some(why.clone()),
                _ => None,
            },
        }
    }
}

/// The session as a caller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SessionInfo {
    pub connections: Vec<ConnectionInfo>,
    pub profiles: Vec<ProfileInfo>,
}

impl From<&Snapshot> for SessionInfo {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            connections: snapshot
                .connections
                .iter()
                .map(ConnectionInfo::from)
                .collect(),
            profiles: snapshot
                .profiles
                .iter()
                .map(|p| ProfileInfo {
                    id: p.id.as_str().to_owned(),
                    name: p.name.clone(),
                    driver: p.kind.as_str().to_owned(),
                })
                .collect(),
        }
    }
}

/// The objects under one connection, as far as they have been loaded.
///
/// Reads the connection's own tree rather than the explorer: whether a row is
/// open is the TUI's navigation state, and a caller sharing the session must
/// not lose sight of a schema because a human collapsed it.
#[must_use]
pub fn tree_of(snapshot: &Snapshot, conn: ConnId) -> Vec<NodeInfo> {
    snapshot.objects(conn).map(NodeInfo::from).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::Value as Json;
    use sqlake_app::action::Action;
    use sqlake_app::store::{Drivers, Store};
    use sqlake_core::id::ProfileId;
    use sqlake_core::node::{NodeKind, NodeRef};
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;

    const LIMIT: Duration = Duration::from_secs(5);

    async fn connected(behaviour: Behaviour) -> (Store, ConnId, Arc<Snapshot>) {
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
        );
        let conn = ConnId::new();
        let snapshot = store
            .dispatch_and_settle(
                Action::Connect {
                    profile: ProfileId::parse("mock").expect("a usable id"),
                    conn,
                },
                LIMIT,
                |s| s.connection_settled(conn),
            )
            .await
            .expect("the connection settles");
        (store, conn, snapshot)
    }

    /// Every key anywhere in the document, however deeply nested.
    fn keys(value: &Json, into: &mut BTreeSet<String>) {
        match value {
            Json::Object(map) => {
                for (key, v) in map {
                    into.insert(key.clone());
                    keys(v, into);
                }
            }
            Json::Array(items) => items.iter().for_each(|v| keys(v, into)),
            _ => {}
        }
    }

    #[tokio::test]
    async fn nothing_crossing_the_socket_carries_a_credential() {
        // Asserted as the whole key set rather than as the absence of a list of
        // bad names: a field added to any of these types fails this test and
        // has to be looked at, which is the only version of this check that
        // keeps working.
        let (_store, _conn, snapshot) = connected(Behaviour::instant()).await;
        let json = serde_json::to_value(SessionInfo::from(&*snapshot)).expect("serialises");

        let mut found = BTreeSet::new();
        keys(&json, &mut found);
        let expected: BTreeSet<String> = [
            "cancel",
            "capabilities",
            "connections",
            "cost_estimate",
            "driver",
            "free_preview",
            "hierarchy",
            "id",
            "name",
            "profile",
            "profiles",
            "sortable_preview",
            "state",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn a_connection_reports_what_it_can_do() {
        let (_store, conn, snapshot) = connected(Behaviour::instant()).await;
        let info = SessionInfo::from(&*snapshot);
        let it = &info.connections[0];
        assert_eq!(it.id, conn.to_string());
        assert_eq!(it.driver, "mock");
        assert_eq!(it.status, Status::Ready);
        let caps = it.capabilities.as_ref().expect("known once open");
        assert_eq!(caps.hierarchy, ["schema", "table"]);
    }

    #[tokio::test]
    async fn a_connection_that_failed_carries_the_reason() {
        let (_store, _conn, snapshot) = connected(Behaviour {
            connect_fails: true,
            ..Behaviour::instant()
        })
        .await;
        let info = SessionInfo::from(&*snapshot);
        let Status::Failed { reason } = &info.connections[0].status else {
            panic!("{:?}", info.connections[0].status);
        };
        assert!(reason.contains("refused"), "{reason}");
        assert!(
            info.connections[0].capabilities.is_none(),
            "nothing was ever learned about it"
        );
    }

    #[tokio::test]
    async fn the_tree_gives_a_path_rather_than_a_name_to_rejoin() {
        let (store, conn, _) = connected(Behaviour::instant()).await;
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);
        let snapshot = store
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

        let nodes = tree_of(&snapshot, conn);
        let schema = nodes.iter().find(|n| n.name == "public").expect("a schema");
        assert_eq!(schema.status, NodeStatus::Loaded);
        let users = nodes
            .iter()
            .find(|n| n.path == ["public", "users"])
            .expect("a relation");
        assert_eq!(users.relation_kind.as_deref(), Some("table"));
        assert_eq!(users.status, NodeStatus::Leaf);
        assert!(users.error.is_none());
    }

    #[tokio::test]
    async fn collapsing_the_row_in_the_tui_does_not_empty_the_agents_tree() {
        // The failure this guards against is silent: a caller sharing the
        // session reads `[]` and concludes the database has no objects,
        // because somebody clicked a triangle.
        let (store, conn, _) = connected(Behaviour::instant()).await;
        let snapshot = store
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

        assert_eq!(snapshot.tree(conn).count(), 0, "the explorer drew nothing");
        assert!(
            tree_of(&snapshot, conn)
                .iter()
                .any(|n| n.path == ["public"]),
            "the objects are still loaded, and the caller has not collapsed anything"
        );
    }

    #[tokio::test]
    async fn a_node_that_failed_reports_the_drivers_message() {
        let (store, conn, _) = connected(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        })
        .await;
        let restricted = NodeRef::new(NodeKind::Namespace, ["restricted"]);
        let snapshot = store
            .dispatch_and_settle(
                Action::ExpandNode {
                    conn,
                    node: restricted.clone(),
                },
                LIMIT,
                |s| s.node_settled(conn, &restricted),
            )
            .await
            .expect("the node settles");

        let nodes = tree_of(&snapshot, conn);
        let it = nodes
            .iter()
            .find(|n| n.path == ["restricted"])
            .expect("the node");
        assert_eq!(it.status, NodeStatus::Failed);
        assert!(
            it.error.as_deref().is_some_and(|e| e.contains("denied")),
            "{it:?}"
        );
    }
}
