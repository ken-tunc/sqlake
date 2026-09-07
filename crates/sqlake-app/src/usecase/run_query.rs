//! Estimate a statement, decide whether it may run, and run it.
//!
//! The three steps are one use case because splitting them would put the gap
//! design.md §4.2 is about back in: "estimated" and "allowed" have to be the
//! same decision, or something eventually runs a query with a number it did
//! not look at.

use async_trait::async_trait;
use sqlake_core::result::ResultSet;
use sqlake_core::sql::{ApprovedQuery, Estimate, InvalidSql, OverBudget, RawSql, ValidatedSql};

use crate::error::{AppError, AppResult};
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct RunQuery {
    pub session: SessionHandle,
}

#[derive(Debug, Clone)]
pub struct RunQueryInput {
    pub sql: RawSql,
    /// Rows to fetch back, or all of them.
    pub max_rows: Option<u32>,
    /// Bytes a query may cost before somebody has to say yes, or `None` for no
    /// ceiling at all.
    pub budget: Option<u64>,
}

/// What came of asking to run something.
///
/// "Needs approval" is a variant here rather than an error, which is design.md
/// §4.2: over the threshold is a normal branch whose answer is a person, and an
/// `Err` would put it on the same footing as a connection that died.
#[derive(Debug)]
pub enum RunQueryOutput {
    Ran {
        estimate: Estimate,
        result: ResultSet,
    },
    /// Carries the statement back out so that approving runs *that* one.
    /// Rebuilding it from the buffer would run whatever the buffer says now,
    /// which after an `$EDITOR` round trip need not be what was estimated.
    NeedsApproval(Box<OverBudget>),
}

#[async_trait]
impl UseCase for RunQuery {
    type Input = RunQueryInput;
    type Output = RunQueryOutput;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        // The dialect comes from the connection this is about to run on, which
        // is the only thing that knows how its server reads a backslash.
        let escaping = self.session.capabilities().escaping;
        let sql = ValidatedSql::parse(&input.sql, escaping).map_err(invalid)?;
        let estimate = self.session.estimate(sql.clone()).await?;

        match ApprovedQuery::within(sql, input.max_rows, estimate, input.budget) {
            Ok(approved) => Ok(RunQueryOutput::Ran {
                estimate,
                result: self.session.execute(approved).await?,
            }),
            Err(over) => Ok(RunQueryOutput::NeedsApproval(Box::new(over))),
        }
    }
}

/// Run something a person has already agreed to.
///
/// A second use case rather than a flag on the first: the input is an
/// `OverBudget` — which only [`RunQuery`] produces — so the one path that
/// skips the budget cannot be reached without having been refused first.
#[derive(Debug)]
pub struct RunApproved {
    pub session: SessionHandle,
}

#[async_trait]
impl UseCase for RunApproved {
    type Input = OverBudget;
    type Output = RunQueryOutput;

    async fn execute(&self, refused: Self::Input) -> AppResult<Self::Output> {
        let estimate = refused.estimate;
        let approved = ApprovedQuery::by_hand(refused);
        Ok(RunQueryOutput::Ran {
            estimate,
            result: self.session.execute(approved).await?,
        })
    }
}

/// A statement this could not accept, as something the user reads.
///
/// Not a `DriverError`: nothing was sent, and reporting it as one would say the
/// server refused something it never saw.
fn invalid(err: InvalidSql) -> AppError {
    AppError::Refused(err.to_string())
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_driver_mock::{Behaviour, ESTIMATES, MockDriver, mock_profile};

    use super::*;

    async fn use_case(
        behaviour: Behaviour,
        capabilities: sqlake_core::capability::Capabilities,
    ) -> RunQuery {
        let driver = MockDriver::new(behaviour).with_capabilities(capabilities);
        RunQuery {
            session: SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap()),
        }
    }

    fn input(sql: &str, budget: Option<u64>) -> RunQueryInput {
        RunQueryInput {
            sql: RawSql::new(sql),
            max_rows: None,
            budget,
        }
    }

    #[tokio::test]
    async fn a_query_runs_and_brings_back_rows() {
        let uc = use_case(Behaviour::instant(), sqlake_driver_mock::CAPABILITIES).await;
        let out = uc
            .execute(input("select * from public.users", None))
            .await
            .unwrap();
        let RunQueryOutput::Ran { result, .. } = out else {
            panic!("should have run: {out:?}");
        };
        assert!(result.row_count() > 0 && result.column_count() > 0);
    }

    #[tokio::test]
    async fn a_query_over_the_budget_stops_and_says_what_it_would_cost() {
        let uc = use_case(
            Behaviour {
                estimate_bytes: 5_000,
                ..Behaviour::instant()
            },
            ESTIMATES,
        )
        .await;
        let out = uc
            .execute(input("select * from public.users", Some(1_000)))
            .await
            .unwrap();
        let RunQueryOutput::NeedsApproval(over) = out else {
            panic!("should have stopped: {out:?}");
        };
        assert_eq!(over.estimate, Estimate::Bytes(5_000));
        assert_eq!(over.budget, 1_000);
    }

    #[tokio::test]
    async fn approving_runs_the_statement_that_was_estimated() {
        let behaviour = Behaviour {
            estimate_bytes: 5_000,
            ..Behaviour::instant()
        };
        let uc = use_case(behaviour.clone(), ESTIMATES).await;
        let out = uc
            .execute(input("select * from public.users", Some(1_000)))
            .await
            .unwrap();
        let RunQueryOutput::NeedsApproval(over) = out else {
            panic!("should have stopped");
        };
        let text = over.sql.text().to_owned();

        let approved = RunApproved {
            session: uc.session.clone(),
        };
        let out = approved.execute(*over).await.unwrap();
        let RunQueryOutput::Ran { result, .. } = out else {
            panic!("approving should run it");
        };
        assert!(result.row_count() > 0);
        assert_eq!(text, "select * from public.users");
    }

    #[tokio::test]
    async fn a_second_statement_is_refused_before_anything_is_sent() {
        // Reported as a refusal rather than a driver error: nothing reached
        // the server, and saying it did would send somebody to look at a log
        // that has nothing in it.
        let uc = use_case(Behaviour::instant(), sqlake_driver_mock::CAPABILITIES).await;
        let err = uc
            .execute(input("drop table x; select 1", None))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Refused(_)), "{err:?}");
        assert!(err.to_string().contains('2'), "{err}");
    }

    #[tokio::test]
    async fn a_driver_that_cannot_estimate_still_runs() {
        // The mock's default set does not estimate, and refusing here would
        // mean nothing runs under CI at all.
        let uc = use_case(Behaviour::instant(), sqlake_driver_mock::CAPABILITIES).await;
        let out = uc
            .execute(input("select * from public.users", Some(0)))
            .await
            .unwrap();
        let RunQueryOutput::Ran { estimate, .. } = out else {
            panic!("a budget it cannot measure against must not stop it");
        };
        assert_eq!(estimate, Estimate::Unknown);
    }

    #[tokio::test]
    async fn a_statement_the_server_refuses_comes_back_as_the_servers_answer() {
        let uc = use_case(
            Behaviour {
                failing_sql: vec!["wrong".to_owned()],
                ..Behaviour::instant()
            },
            sqlake_driver_mock::CAPABILITIES,
        )
        .await;
        let err = uc.execute(input("select wrong", None)).await.unwrap_err();
        assert!(matches!(err, AppError::Driver(_)), "{err:?}");
    }
}
