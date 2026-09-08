//! What a statement would cost, without running it.
//!
//! Separate from [`RunQuery`](crate::usecase::RunQuery) because the answer is
//! the whole point rather than a step: an agent surfacing a number to a person
//! wants it *and no query*, and reaching it through the running path would mean
//! asking for something and hoping the budget said no.
//!
//! It validates the same way and refuses a write on a read-only connection the
//! same way. Estimating something that could never run would answer a question
//! about a statement this connection will not accept.

use async_trait::async_trait;
use sqlake_core::sql::{Estimate, RawSql, StatementKind, ValidatedSql};

use crate::error::AppResult;
use crate::session::SessionHandle;
use crate::usecase::UseCase;
use crate::usecase::run_query::{invalid, refused_write};

#[derive(Debug)]
pub struct EstimateQuery {
    pub session: SessionHandle,
}

#[derive(Debug, Clone)]
pub struct EstimateQueryInput {
    pub sql: RawSql,
    pub access: sqlake_core::sql::Access,
}

#[async_trait]
impl UseCase for EstimateQuery {
    type Input = EstimateQueryInput;
    type Output = Estimate;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        let escaping = self.session.capabilities().escaping;
        let sql = ValidatedSql::parse(&input.sql, escaping).map_err(invalid)?;
        if input.access == sqlake_core::sql::Access::ReadOnly && sql.kind() == StatementKind::Writes
        {
            return Err(refused_write(&sql));
        }
        self.session.estimate(sql).await
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_core::sql::Access;
    use sqlake_driver_mock::{Behaviour, ESTIMATES, MockDriver, mock_profile};

    use super::*;
    use crate::error::AppError;

    async fn use_case(behaviour: Behaviour) -> EstimateQuery {
        let driver = MockDriver::new(behaviour).with_capabilities(ESTIMATES);
        EstimateQuery {
            session: SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap()),
        }
    }

    #[tokio::test]
    async fn a_statement_is_costed_without_being_run() {
        let uc = use_case(Behaviour {
            estimate_bytes: 4096,
            ..Behaviour::instant()
        })
        .await;
        let estimate = uc
            .execute(EstimateQueryInput {
                sql: RawSql::new("select * from public.users"),
                access: Access::ReadWrite,
            })
            .await
            .unwrap();
        assert_eq!(estimate, Estimate::Bytes(4096));
    }

    #[tokio::test]
    async fn a_write_is_refused_on_a_read_only_connection() {
        // Estimating something that could never run answers a question about
        // a statement this connection will not accept.
        let uc = use_case(Behaviour::instant()).await;
        let err = uc
            .execute(EstimateQueryInput {
                sql: RawSql::new("delete from public.users"),
                access: Access::ReadOnly,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Refused(_)), "{err:?}");
    }

    #[tokio::test]
    async fn two_statements_are_refused_before_anything_is_sent() {
        let uc = use_case(Behaviour::instant()).await;
        let err = uc
            .execute(EstimateQueryInput {
                sql: RawSql::new("select 1; select 2"),
                access: Access::ReadWrite,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Refused(_)), "{err:?}");
    }
}
