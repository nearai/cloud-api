//! Per-connection prepared-statement reuse for hot-path queries.
//!
//! `tokio_postgres` prepares a raw SQL string on every call, so each query is
//! two network round trips (Parse/Describe, then Bind/Execute). deadpool keeps
//! a per-connection statement cache; going through it makes a repeated query
//! one round trip. On a replica far from the database this is the difference
//! between ~60 ms and ~120 ms per query.
//!
//! The one thing a cached statement cannot survive is a change to its result
//! shape: after a migration adds a column to a table read with `SELECT *`,
//! Postgres rejects the old plan with SQLSTATE 0A000 "cached plan must not
//! change result type". That happens on replicas still running the previous
//! release during a rolling deploy. The helpers below drop the stale statement
//! from the connection's cache when they see that error, and
//! [`map_db_error`](super::utils::map_db_error) classifies it as retryable so
//! the surrounding `retry_db!` block re-prepares and re-runs the statement.

use async_trait::async_trait;
use deadpool_postgres::{Client, Transaction};
use tokio_postgres::{error::SqlState, types::ToSql, Error, Row};

const STALE_PLAN_MESSAGE: &str = "cached plan must not change result type";

/// True when `err` is Postgres refusing a prepared statement whose result
/// columns changed since it was prepared.
pub fn is_stale_cached_plan(err: &Error) -> bool {
    err.as_db_error().is_some_and(|db_err| {
        db_err.code() == &SqlState::FEATURE_NOT_SUPPORTED
            && db_err.message().contains(STALE_PLAN_MESSAGE)
    })
}

/// Run queries through the connection's prepared-statement cache.
#[async_trait]
pub trait CachedStatements {
    async fn cached_query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, Error>;

    async fn cached_query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, Error>;

    async fn cached_query_one(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, Error>;

    async fn cached_execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)])
        -> Result<u64, Error>;
}

macro_rules! impl_cached_statements {
    ($ty:ty) => {
        #[async_trait]
        impl CachedStatements for $ty {
            async fn cached_query(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Vec<Row>, Error> {
                let statement = self.prepare_cached(sql).await?;
                let result = self.query(&statement, params).await;
                self.forget_if_stale(sql, &result);
                result
            }

            async fn cached_query_opt(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Option<Row>, Error> {
                let statement = self.prepare_cached(sql).await?;
                let result = self.query_opt(&statement, params).await;
                self.forget_if_stale(sql, &result);
                result
            }

            async fn cached_query_one(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Row, Error> {
                let statement = self.prepare_cached(sql).await?;
                let result = self.query_one(&statement, params).await;
                self.forget_if_stale(sql, &result);
                result
            }

            async fn cached_execute(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<u64, Error> {
                let statement = self.prepare_cached(sql).await?;
                let result = self.execute(&statement, params).await;
                self.forget_if_stale(sql, &result);
                result
            }
        }

        impl StaleStatementCleanup for $ty {
            fn forget_if_stale<T>(&self, sql: &str, result: &Result<T, Error>) {
                if let Err(err) = result {
                    if is_stale_cached_plan(err) {
                        tracing::warn!(
                            "Dropping prepared statement whose result shape changed; retrying"
                        );
                        self.statement_cache.remove(sql, &[]);
                    }
                }
            }
        }
    };
}

trait StaleStatementCleanup {
    fn forget_if_stale<T>(&self, sql: &str, result: &Result<T, Error>);
}

impl_cached_statements!(Client);
impl_cached_statements!(Transaction<'_>);
