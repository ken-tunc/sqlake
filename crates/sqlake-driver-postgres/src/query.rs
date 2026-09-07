//! Running a statement, and asking the planner what it will cost.
//!
//! The text is the user's own and is sent unaltered. Nothing here appends a
//! `LIMIT`: the row cap is applied to the fetch, so the statement PostgreSQL
//! sees — and therefore the offsets in any error it reports — is the one that
//! was typed.

use futures::{StreamExt as _, pin_mut};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::sql::{ApprovedQuery, Estimate, ValidatedSql};
use tokio::sync::OwnedMutexGuard;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::tls;
use crate::value::RawValue;

/// The planner's total cost for the statement.
///
/// Cost units, not bytes and not money: they order two plans on one server and
/// mean nothing anywhere else, which is why [`Estimate`] keeps them apart from
/// the number BigQuery gives.
///
/// `EXPLAIN` without `ANALYZE`, so the statement is planned and not run — the
/// distinction the whole estimate rests on.
///
/// A statement `EXPLAIN` will not take is [`Estimate::Unknown`], not an error:
/// PostgreSQL only plans optimizable statements, so `create table`, `set`,
/// `begin`, `grant` and every other utility statement come back as a syntax
/// error here. Refusing on that would make them impossible to run at all —
/// [`Session::execute`] is reachable only through an estimate — and would blame
/// the user's statement for a wrapper they never wrote. What the server thinks
/// of the statement is answered by sending it.
///
/// [`Session::execute`]: sqlake_core::driver::Session::execute
pub async fn estimate(client: &Client, sql: &ValidatedSql) -> DriverResult<Estimate> {
    let text = format!("EXPLAIN (FORMAT JSON) {}", sql.text());
    let rows = match client.query(&text, &[]).await {
        Ok(rows) => rows,
        // Logged rather than silent. Most of these are a utility statement,
        // which is ordinary; a connection that has died is not, and it would
        // otherwise reappear as a puzzling failure one round trip later with
        // nothing to say it had happened twice.
        Err(err) => {
            tracing::debug!(error = %crate::describe(&err), "EXPLAIN would not take the statement");
            return Ok(Estimate::Unknown);
        }
    };

    let plan: Option<serde_json::Value> = rows.first().and_then(|row| row.try_get(0).ok());
    Ok(plan
        .as_ref()
        .and_then(total_cost)
        .map_or(Estimate::Unknown, Estimate::Cost))
}

/// `[{"Plan": {"Total Cost": 1.23, ...}}]`, which is the shape every version
/// since 9.0 produces. Unknown rather than an error when it is not: a plan this
/// could not read is a reason to say nothing about the cost, not to refuse to
/// run the query.
fn total_cost(plan: &serde_json::Value) -> Option<f64> {
    plan.get(0)?.get("Plan")?.get("Total Cost")?.as_f64()
}

/// Tells the server to stop, if it is dropped while a query is still running.
///
/// A cancel request is a *second connection* carrying the backend's process id
/// and secret — it cannot be sent down the socket the query is occupying, which
/// is exactly why it works while the first one is busy.
///
/// Dropping is the signal because dropping is what already happens: the layer
/// above stops awaiting, the future unwinds, and this runs. Asking the driver
/// to take a cancellation handle instead would mean threading one through a
/// trait so that it could be triggered from a task that has already let go.
struct StopsTheQuery {
    token: Option<CancelToken>,
    tls: Option<tls::Verification>,
    /// The connection's lock, handed to the cancel request so that the next
    /// call on this connection waits for it. A cancel names the backend rather
    /// than the statement, so one still opening its socket when the next query
    /// starts would cancel *that*.
    running: Option<OwnedMutexGuard<()>>,
}

impl Drop for StopsTheQuery {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        let tls = self.tls;
        let running = self.running.take();
        // Spawned, because `Drop` cannot await. A runtime already shutting
        // down refuses it, and there is nothing useful to do about that: the
        // process is going, and the server drops the query with the socket.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            // Dropped at the end of this task, not before: holding it is what
            // makes the next caller wait. The connect is bounded by the
            // profile's `connect_timeout`, so the wait is too.
            let _running = running;
            // The connection this goes down has to be secured the same way the
            // first one was: a server requiring TLS refuses a plaintext cancel
            // request, and one that is not expecting TLS refuses the handshake.
            let sent = match tls {
                None => token.cancel_query(tokio_postgres::NoTls).await,
                Some(verification) => match tls::client_config(verification) {
                    Ok(config) => token.cancel_query(MakeRustlsConnect::new(config)).await,
                    Err(why) => {
                        tracing::warn!(%why, "cancel: no TLS configuration to send it with");
                        return;
                    }
                },
            };
            match sent {
                // PostgreSQL answers nothing at all to a cancel request — it
                // acts on it or ignores it, and never says which — so this
                // says only that the request went out.
                Ok(()) => tracing::info!("asked postgres to cancel the running query"),
                Err(err) => tracing::warn!(error = %crate::describe(&err), "cancel request failed"),
            }
        });
    }
}

impl StopsTheQuery {
    /// The query finished on its own, so there is nothing to cancel.
    fn disarm(&mut self) {
        self.token = None;
    }
}

pub async fn execute(
    client: &Client,
    tls: Option<tls::Verification>,
    running: OwnedMutexGuard<()>,
    query: &ApprovedQuery,
) -> DriverResult<ResultSet> {
    // Prepared first, so the columns come from the *statement*. Reading them
    // off the first row costs nothing until the result is empty, and then the
    // grid is handed a result with no columns and draws nothing at all — which
    // looks like a failure rather than like a query that matched no rows.
    let statement = client
        .prepare(query.text())
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;

    let columns: Vec<Column> = statement
        .columns()
        .iter()
        // Nullability is not on the wire and a second round trip to ask for it
        // is not worth it here either; `describe` in M5 is where a column list
        // becomes authoritative.
        .map(|column| Column::new(column.name(), column.type_().name(), true))
        .collect();

    // Armed *before* the statement goes out, not after the await that sends
    // it: Bind, Execute and Sync are pipelined, so the server can already be
    // running the query while this side is still waiting to hear that the bind
    // succeeded. A guard armed after that await is unarmed for part of the
    // window it exists for. The cost of arming early is at most a cancel
    // request for a query that never ran, which the server ignores.
    let mut guard = StopsTheQuery {
        token: Some(client.cancel_token()),
        tls,
        running: Some(running),
    };

    // `query_raw` rather than `query`: it streams, so a cap of fifty on a
    // statement matching a million rows stops after fifty instead of
    // materialising the lot and throwing most of it away.
    let stream = client
        .query_raw(&statement, std::iter::empty::<&str>())
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;
    pin_mut!(stream);

    let cap = query.max_rows().map_or(usize::MAX, |n| n as usize);
    let mut rows: Vec<Row> = Vec::new();
    // Before the await, not after: asking for one more row than the cap and
    // then discarding it is a round trip nobody wanted, and with a cap of zero
    // it is the *only* round trip.
    while rows.len() < cap
        && let Some(row) = stream.next().await
    {
        let row = row.map_err(|err| DriverError::Query(crate::describe(&err)))?;
        rows.push(
            (0..row.len())
                .map(|i| row.get::<_, RawValue>(i).decode())
                .collect(),
        );
    }

    // Nobody is waiting on this statement any more — every row that was asked
    // for has arrived, and under a cap the rest are drained with the stream.
    // A failure inside the loop above leaves it armed, which sends a request
    // the server ignores — the alternative is deciding whether each error
    // means the statement ended, which is a question only the server can
    // answer and which it answers by ignoring the request.
    guard.disarm();

    // Uncapped, the stream ran to the end and the count is the whole answer.
    // Capped, it is not: `None` rather than fifty, which the grid would show as
    // "50 rows" for a query that matched a million.
    let total = query.max_rows().is_none().then_some(rows.len() as u64);
    Ok(ResultSet::new(columns, rows, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_gives_up_its_total_cost() {
        let plan =
            serde_json::json!([{ "Plan": { "Node Type": "Seq Scan", "Total Cost": 155.0 } }]);
        assert_eq!(total_cost(&plan), Some(155.0));
    }

    #[test]
    fn a_plan_this_cannot_read_says_nothing_rather_than_guessing() {
        // A shape from a version that changed, or an `EXPLAIN` of something
        // with no plan. Refusing to run the query over it would be a worse
        // answer than having no number.
        for odd in [
            serde_json::json!([]),
            serde_json::json!([{ "Plan": {} }]),
            serde_json::json!({ "Plan": { "Total Cost": 1.0 } }),
            serde_json::json!([{ "Plan": { "Total Cost": "cheap" } }]),
        ] {
            assert_eq!(total_cost(&odd), None, "{odd}");
        }
    }
}
