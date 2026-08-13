//! Fetch one level of the object tree.

use async_trait::async_trait;
use sqlake_core::node::{NodeRef, TreeNode};

use crate::error::AppResult;
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct ExpandNode {
    pub session: SessionHandle,
}

#[derive(Debug, Clone)]
pub struct ExpandNodeInput {
    pub node: NodeRef,
}

#[derive(Debug)]
pub struct ExpandNodeOutput {
    /// Echoed back so a late reply can be matched to the node that asked for
    /// it, rather than applied to whatever is selected now.
    pub node: NodeRef,
    pub children: Vec<TreeNode>,
}

#[async_trait]
impl UseCase for ExpandNode {
    type Input = ExpandNodeInput;
    type Output = ExpandNodeOutput;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        let children = self.session.children(input.node.clone()).await?;
        Ok(ExpandNodeOutput {
            node: input.node,
            children,
        })
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_core::node::NodeKind;
    use sqlake_driver_mock::{Behaviour, MockDriver};

    use super::*;
    use crate::error::AppError;

    async fn use_case(behaviour: Behaviour) -> ExpandNode {
        let driver = MockDriver::new(behaviour);
        ExpandNode {
            session: SessionHandle::spawn(driver.connect().await.unwrap()),
        }
    }

    #[tokio::test]
    async fn expanding_a_schema_returns_its_relations() {
        let uc = use_case(Behaviour::instant()).await;
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        let out = uc
            .execute(ExpandNodeInput { node: node.clone() })
            .await
            .unwrap();

        assert_eq!(out.node, node, "the reply must identify its own request");
        assert!(out.children.iter().any(|c| c.label == "users"));
    }

    #[tokio::test]
    async fn a_failing_node_reports_the_driver_message() {
        let uc = use_case(Behaviour {
            failing_nodes: vec![vec!["restricted".to_owned()]],
            ..Behaviour::instant()
        })
        .await;
        let err = uc
            .execute(ExpandNodeInput {
                node: NodeRef::new(NodeKind::Namespace, ["restricted"]),
            })
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Driver(_)));
        assert!(err.user_message().contains("permission denied"));
    }
}
