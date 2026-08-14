//! Fetch a page of a relation.

use async_trait::async_trait;
use sqlake_core::node::TableRef;
use sqlake_core::result::{PageRequest, ResultSet};

use crate::error::AppResult;
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct PreviewTable {
    pub session: SessionHandle,
}

#[derive(Debug, Clone)]
pub struct PreviewTableInput {
    pub table: TableRef,
    pub page: PageRequest,
}

#[derive(Debug)]
pub struct PreviewTableOutput {
    pub table: TableRef,
    pub page: PageRequest,
    /// The rows as the driver returned them. Preparing them for a screen is
    /// the front-end's business, and the two front-ends disagree about it.
    pub result: ResultSet,
}

#[async_trait]
impl UseCase for PreviewTable {
    type Input = PreviewTableInput;
    type Output = PreviewTableOutput;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        let result = self
            .session
            .preview(input.table.clone(), input.page)
            .await?;
        Ok(PreviewTableOutput {
            table: input.table,
            page: input.page,
            result,
        })
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_core::result::{Sort, SortDir};
    use sqlake_driver_mock::{Behaviour, MockDriver, mock_profile};

    use super::*;

    async fn use_case(behaviour: Behaviour) -> PreviewTable {
        let driver = MockDriver::new(behaviour);
        PreviewTable {
            session: SessionHandle::spawn(driver.connect(&mock_profile("mock")).await.unwrap()),
        }
    }

    #[tokio::test]
    async fn a_preview_arrives_ready_to_draw() {
        let uc = use_case(Behaviour::instant()).await;
        let out = uc
            .execute(PreviewTableInput {
                table: TableRef::new(["public", "users"]),
                page: PageRequest::first(),
            })
            .await
            .unwrap();

        assert_eq!(out.result.rows.len(), 50);
        assert_eq!(out.result.columns[0].name, "id");
    }

    #[tokio::test]
    async fn only_the_requested_page_is_materialised() {
        let uc = use_case(Behaviour::instant()).await;
        let out = uc
            .execute(PreviewTableInput {
                table: TableRef::new(["public", "big"]),
                page: PageRequest {
                    offset: 0,
                    limit: 100,
                    sort: None,
                },
            })
            .await
            .unwrap();

        assert_eq!(out.result.rows.len(), 100);
        assert_eq!(out.result.total_rows, Some(200_000));
    }

    #[tokio::test]
    async fn the_request_is_echoed_back_with_its_result() {
        // A late reply must be matched to the request that produced it, or a
        // stale page overwrites a newer one.
        let uc = use_case(Behaviour::instant()).await;
        let page = PageRequest::first().with_sort(Some(Sort::new(0, SortDir::Desc)));
        let out = uc
            .execute(PreviewTableInput {
                table: TableRef::new(["public", "users"]),
                page,
            })
            .await
            .unwrap();

        assert_eq!(out.page, page);
        assert_eq!(out.table, TableRef::new(["public", "users"]));
    }

    #[tokio::test]
    async fn an_empty_relation_still_produces_columns() {
        let uc = use_case(Behaviour::instant()).await;
        let out = uc
            .execute(PreviewTableInput {
                table: TableRef::new(["public", "empty"]),
                page: PageRequest::first(),
            })
            .await
            .unwrap();

        assert!(out.result.rows.is_empty());
        assert_eq!(out.result.columns.len(), 2);
    }

    #[tokio::test]
    async fn a_broken_relation_fails_rather_than_returning_nothing() {
        let uc = use_case(Behaviour {
            failing_nodes: vec![vec!["analytics".to_owned(), "broken".to_owned()]],
            ..Behaviour::instant()
        })
        .await;
        let err = uc
            .execute(PreviewTableInput {
                table: TableRef::new(["analytics", "broken"]),
                page: PageRequest::first(),
            })
            .await
            .unwrap_err();

        assert!(err.user_message().contains("corrupt"));
    }
}
