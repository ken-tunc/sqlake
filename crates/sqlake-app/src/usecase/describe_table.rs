//! What a relation is, fetched once.

use async_trait::async_trait;
use sqlake_core::detail::TableDetail;
use sqlake_core::node::TableRef;

use crate::error::AppResult;
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct DescribeTable {
    pub session: SessionHandle,
}

#[derive(Debug, Clone)]
pub struct DescribeTableInput {
    pub table: TableRef,
}

#[async_trait]
impl UseCase for DescribeTable {
    type Input = DescribeTableInput;
    type Output = TableDetail;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        self.session.describe(input.table).await
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_driver_mock::{Behaviour, MockDriver, mock_profile};

    use super::*;

    async fn use_case() -> DescribeTable {
        let driver = MockDriver::new(Behaviour::instant());
        DescribeTable {
            session: SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap()),
        }
    }

    #[tokio::test]
    async fn a_relation_comes_back_described() {
        let detail = use_case()
            .await
            .execute(DescribeTableInput {
                table: TableRef::new(["public", "users"]),
            })
            .await
            .unwrap();
        assert!(!detail.columns.is_empty());
        assert_eq!(detail.table, TableRef::new(["public", "users"]));
    }

    #[tokio::test]
    async fn a_relation_that_is_not_there_is_an_error_rather_than_an_empty_definition() {
        let err = use_case()
            .await
            .execute(DescribeTableInput {
                table: TableRef::new(["public", "nope"]),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, crate::error::AppError::Driver(_)), "{err:?}");
    }
}
