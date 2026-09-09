//! What a caller is told about the session's state.
//!
//! Nothing here carries a host, a user, or anything derived from a credential.
//! `Snapshot` holds no `ResolvedProfile` today, and serialisation must not be
//! the thing that changes that: a connection is an id, a name, a driver and a
//! status, and the socket is the one place where an accidental field would
//! leave the process.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlake_app::PagedResult;
use sqlake_app::snapshot::{ConnStatus, ConnectionView, LoadState, QueryView, Snapshot};
use sqlake_app::tree::{NodeState, VisibleNode};
use sqlake_core::capability::Capabilities;
use sqlake_core::detail::{ColumnDef, TableDetail};
use sqlake_core::id::ConnId;
use sqlake_core::library::Template;
use sqlake_core::sql::{Estimate, Position};
use sqlake_core::template::{Dialect, Kind, Placeholder, placeholders};

use crate::page::{Budget, Page};

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
    use std::sync::Arc;
    use std::time::Duration;

    use sqlake_app::action::Action;
    use sqlake_app::store::{Drivers, Store, Wiring};
    use sqlake_core::id::ProfileId;
    use sqlake_core::node::{NodeKind, NodeRef};
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;

    const LIMIT: Duration = Duration::from_secs(5);

    async fn connected(behaviour: Behaviour) -> (Store, ConnId, Arc<Snapshot>) {
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
        ));
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

/// Where in a statement a failure was, when the server said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct PositionInfo {
    pub line: u32,
    pub column: u32,
}

impl From<Position> for PositionInfo {
    fn from(at: Position) -> Self {
        Self {
            line: at.line,
            column: at.column,
        }
    }
}

/// What a query was expected to cost.
///
/// Tagged by what was measured rather than flattened to a number, because the
/// two are not comparable: BigQuery's bytes turn into money and PostgreSQL's
/// planner cost units do not, and a caller handed `{"cost": 155.0}` with no
/// unit would be entitled to treat them the same way.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "measured")]
pub enum EstimateInfo {
    /// Bytes the query will be billed for.
    Bytes { bytes: u64 },
    /// The planner's own units. Comparable between two plans on one server and
    /// meaningless anywhere else, so nothing gates on it.
    Cost { cost: f64 },
    /// This driver does not estimate. `capabilities.cost_estimate` says so in
    /// advance, so it is expected rather than a failure.
    Unknown {},
}

impl From<Estimate> for EstimateInfo {
    fn from(estimate: Estimate) -> Self {
        match estimate {
            Estimate::Bytes(bytes) => Self::Bytes { bytes },
            Estimate::Cost(cost) => Self::Cost { cost },
            Estimate::Unknown => Self::Unknown {},
        }
    }
}

/// Where a query has got to.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum QueryState {
    /// Being estimated, or running. `query wait` is what blocks on it.
    Working,
    /// Costed and not run, which is all `query estimate` asks for.
    Estimated,
    /// Over the budget, and stopped.
    ///
    /// Not an error: design.md §4.2. A caller cannot approve it itself — the
    /// number goes to a person, who says yes in the TUI or by re-running under
    /// a budget that allows it.
    NeedsApproval {
        budget: u64,
    },
    Ready {
        page: Page,
    },
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<PositionInfo>,
    },
}

/// One run of a statement, and what has come of it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct QueryInfo {
    pub id: String,
    pub connection: String,
    /// The statement, as it was sent. Kept so a result can be read next to
    /// what produced it.
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<EstimateInfo>,
    #[serde(flatten)]
    pub state: QueryState,
}

impl QueryInfo {
    /// The wire form of a query, with its rows cut to what a caller can hold.
    #[must_use]
    pub fn of(query: &QueryView, budget: Budget) -> Self {
        let state = match (&query.needs_approval, &query.data) {
            (Some(over), _) => QueryState::NeedsApproval {
                budget: over.budget,
            },
            (_, LoadState::Loading) => QueryState::Working,
            (_, LoadState::Ready(rows)) => QueryState::Ready {
                page: Page::of(rows, budget),
            },
            (_, LoadState::Failed(message)) => QueryState::Failed {
                message: message.clone(),
                at: query.failed_at.map(PositionInfo::from),
            },
            // Idle is "nothing was requested", which after an estimate is the
            // literal truth: no rows were asked for.
            (_, LoadState::Idle) => QueryState::Estimated,
        };
        Self {
            id: query.id.to_string(),
            connection: query.conn.to_string(),
            sql: query.sql.clone(),
            estimate: query.estimate.map(EstimateInfo::from),
            state,
        }
    }

    /// Whether this is an answer rather than a progress report.
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        !matches!(self.state, QueryState::Working)
    }
}

/// One column, as the catalogue describes it.
///
/// `nullable` is a boolean here and the word "not null" in the TUI. That is
/// the whole reason [`sqlake_app`] holds neither: a reader that branches on
/// the answer wants the boolean, and a reader looking at a grid wants the
/// word.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct ColumnDefInfo {
    pub name: String,
    /// The driver's own name for the type, uninterpreted.
    pub type_name: String,
    pub nullable: bool,
    /// The default expression as the server stores it — `now()` rather than a
    /// value, because that is what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl From<&ColumnDef> for ColumnDefInfo {
    fn from(column: &ColumnDef) -> Self {
        Self {
            name: column.name.clone(),
            type_name: column.type_name.clone(),
            nullable: column.nullable,
            default: column.default.clone(),
            comment: column.comment.clone(),
        }
    }
}

/// One table of facts about a relation, under the driver's own title.
///
/// A [`Page`] rather than a shape of its own: an index list and a query result
/// are both rows under columns, and a caller that can read one can read the
/// other without being taught a second format.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct SectionInfo {
    pub title: String,
    pub page: Page,
}

/// One named fact about a relation — a row count, a size, a modification time.
///
/// An object rather than the pair it is in the driver, because a two-element
/// array on the wire is a shape a reader has to be told the meaning of.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct StatInfo {
    pub name: String,
    pub value: String,
}

/// What a relation is, as opposed to what is in it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct DefinitionInfo {
    pub table: Vec<String>,
    pub relation_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub columns: Vec<ColumnDefInfo>,
    /// Columns left out by the budget, named rather than counted, for the
    /// reason [`Page::omitted_columns`] is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_columns: Vec<String>,
    /// Whatever this driver has — indexes, triggers, partitioning. Empty is a
    /// normal answer rather than a gap: BigQuery has no triggers to have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SectionInfo>,
    /// Built here from the catalogue, not the statement the relation was
    /// created with — which is what the field is named for. Neither server
    /// hands the original over for free, and running this covers what it
    /// covers and no more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_ddl: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stats: Vec<StatInfo>,
}

impl DefinitionInfo {
    /// The wire form, cut to what a caller can hold.
    ///
    /// Columns are cut by the *row* budget rather than the column one: a
    /// thousand-column table is a thousand rows to somebody reading its
    /// definition, and `max_columns` is about how wide one row may be.
    #[must_use]
    pub fn of(detail: &TableDetail, budget: Budget) -> Self {
        let kept = detail.columns.len().min(budget.max_rows);
        Self {
            table: detail.table.path.clone(),
            relation_kind: detail.kind.as_str().to_owned(),
            comment: detail.comment.clone(),
            columns: detail.columns[..kept]
                .iter()
                .map(ColumnDefInfo::from)
                .collect(),
            omitted_columns: detail.columns[kept..]
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            sections: detail
                .sections
                .iter()
                .map(|section| SectionInfo {
                    title: section.title.clone(),
                    page: Page::of(&PagedResult::new(&section.table), budget),
                })
                .collect(),
            generated_ddl: detail.ddl.as_ref().map(|d| d.text().to_owned()),
            stats: detail
                .stats
                .iter()
                .map(|(name, value)| StatInfo {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }
}

/// One placeholder a template asks for.
///
/// Listed with the template so a caller need not find `{{…}}` in the body
/// itself: two readers of one template disagreeing about what it asks for is
/// exactly what a second parser produces.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct PlaceholderInfo {
    pub name: String,
    /// `value` or `ident`. It decides how the answer is quoted, and therefore
    /// what kind of thing to send: a name here, a value there.
    pub kind: String,
}

impl From<&Placeholder> for PlaceholderInfo {
    fn from(placeholder: &Placeholder) -> Self {
        Self {
            name: placeholder.name.clone(),
            kind: match placeholder.kind {
                Kind::Value => "value",
                Kind::Ident => "ident",
            }
            .to_owned(),
        }
    }
}

/// A saved statement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct TemplateInfo {
    /// What `template_apply` names it by. Unique, which is what makes it the
    /// name rather than the row id.
    pub name: String,
    pub body: String,
    /// The driver it is written for, or absent for one that works on any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placeholders: Vec<PlaceholderInfo>,
    /// Why the body could not be read, when it could not.
    ///
    /// Said rather than dropped: a template with an unterminated string in it
    /// is still saved and still shown, and a caller told only that it has no
    /// placeholders would go on to apply it and be refused for a reason it
    /// could have been given here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,
}

impl TemplateInfo {
    /// The wire form, with the placeholders worked out under `dialect`.
    #[must_use]
    pub fn of(template: &Template, dialect: Dialect) -> Self {
        let (placeholders, unreadable) = match placeholders(&template.body, dialect) {
            Ok(found) => (found.iter().map(PlaceholderInfo::from).collect(), None),
            Err(why) => (Vec::new(), Some(why.to_string())),
        };
        Self {
            name: template.name.clone(),
            body: template.body.clone(),
            driver: template.driver.map(|kind| kind.as_str().to_owned()),
            tags: template.tags.clone(),
            placeholders,
            unreadable,
        }
    }
}

/// A template with its placeholders filled in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct StatementInfo {
    /// Which template it came from, so an answer can be read next to the
    /// thing that produced it.
    pub template: String,
    /// The statement, quoted for the connection it was built against. Not run
    /// — `query_run` is what runs one.
    pub sql: String,
}
