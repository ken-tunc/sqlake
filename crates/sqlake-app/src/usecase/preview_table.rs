//! Fetch a page of a relation and prepare it for display.

use async_trait::async_trait;
use sqlake_core::node::TableRef;
use sqlake_core::result::PageRequest;

use crate::error::AppResult;
use crate::grid::RenderedGrid;
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
    /// Already prepared for display: the UI never sees a `Value`.
    pub grid: RenderedGrid,
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
            grid: RenderedGrid::new(result),
        })
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::driver::Driver;
    use sqlake_core::result::{Sort, SortDir};
    use sqlake_driver_mock::{Behaviour, MockDriver};

    use super::*;
    use crate::grid::Align;

    async fn use_case(behaviour: Behaviour) -> PreviewTable {
        let driver = MockDriver::new(behaviour);
        PreviewTable {
            session: SessionHandle::spawn(driver.connect().await.unwrap()),
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

        assert_eq!(out.grid.row_count(), 50);
        assert_eq!(out.grid.columns()[0].name, "id");
        assert_eq!(out.grid.columns()[0].align, Align::Right);
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

        assert_eq!(out.grid.row_count(), 100);
        assert_eq!(out.grid.total_rows(), Some(200_000));
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

        assert!(out.grid.is_empty());
        assert_eq!(out.grid.columns().len(), 2);
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
