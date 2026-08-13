//! Open a connection and load the top level of its tree.

use std::sync::Arc;

use async_trait::async_trait;
use sqlake_core::capability::Capabilities;
use sqlake_core::driver::Driver;
use sqlake_core::node::{NodeRef, TreeNode};

use crate::error::AppResult;
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct Connect {
    pub driver: Arc<dyn Driver>,
}

#[derive(Debug, Clone)]
pub struct ConnectInput {
    /// What the connection is called in the UI.
    pub name: String,
}

#[derive(Debug)]
pub struct ConnectOutput {
    pub name: String,
    pub session: SessionHandle,
    pub capabilities: Capabilities,
    /// The top level, fetched here so the tree is never briefly empty after a
    /// successful connection.
    pub roots: Vec<TreeNode>,
}

#[async_trait]
impl UseCase for Connect {
    type Input = ConnectInput;
    type Output = ConnectOutput;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        let session = SessionHandle::spawn(self.driver.connect().await?);
        let capabilities = session.capabilities();
        let roots = session.children(NodeRef::root()).await?;
        Ok(ConnectOutput {
            name: input.name,
            session,
            capabilities,
            roots,
        })
    }
}

#[cfg(test)]
mod tests {
    use sqlake_driver_mock::{Behaviour, MockDriver};

    use super::*;

    #[tokio::test]
    async fn connecting_also_loads_the_top_level() {
        let uc = Connect {
            driver: Arc::new(MockDriver::new(Behaviour::instant())),
        };
        let out = uc
            .execute(ConnectInput {
                name: "mock".into(),
            })
            .await
            .unwrap();

        assert_eq!(out.name, "mock");
        assert_eq!(out.capabilities.hierarchy.len(), 2);
        // A connection that succeeds but shows an empty tree looks broken.
        assert!(!out.roots.is_empty());
    }
}
