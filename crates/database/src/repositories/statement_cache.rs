//! Per-connection prepared-statement reuse for hot-path queries.
//!
//! `tokio_postgres` prepares a raw SQL string on every call, so each query is
//! two network round trips (Parse/Describe, then Bind/Execute). deadpool keeps
//! a per-connection statement cache; going through it makes a repeated query
//! one round trip. On a replica far from the database this is the difference
//! between ~60 ms and ~120 ms per query.
//!
//! A cached statement can outlive the server-side object it points at:
//!
//! - after a migration adds a column to a table read with `SELECT *`, Postgres
//!   refuses the old plan with SQLSTATE 0A000 "cached plan must not change
//!   result type" (a replica still on the previous release during a rolling
//!   deploy hits this on every warm connection);
//! - a statement can be missing on the server altogether, SQLSTATE 26000,
//!   if the connection was reset underneath the cache.
//!
//! Both are handled the same way: the stale entry is evicted from every
//! connection's cache in the pool (a schema change invalidates all of them
//! at once), the statement is re-prepared and re-run on the same connection
//! when no transaction is involved, and [`map_db_error`](super::utils::map_db_error)
//! classifies the error as retryable so `retry_db!` re-runs transactional
//! blocks with a fresh statement.

use async_trait::async_trait;
use deadpool_postgres::{Client, Pool, Transaction};
use tokio_postgres::{error::SqlState, types::ToSql, Error, Row};

/// PostgreSQL routine that raises the stale-plan error; stable across server
/// locales, unlike the message text.
const STALE_PLAN_ROUTINE: &str = "RevalidateCachedQuery";
const STALE_PLAN_MESSAGE: &str = "cached plan must not change result type";

/// True when `err` is Postgres refusing a prepared statement whose result
/// columns changed since it was prepared.
pub fn is_stale_cached_plan(err: &Error) -> bool {
    err.as_db_error().is_some_and(|db_err| {
        db_err.code() == &SqlState::FEATURE_NOT_SUPPORTED
            && (db_err.routine() == Some(STALE_PLAN_ROUTINE)
                || db_err.message().contains(STALE_PLAN_MESSAGE))
    })
}

/// True when the server no longer has the prepared statement the cache
/// handed out.
pub fn is_missing_prepared_statement(err: &Error) -> bool {
    err.as_db_error()
        .is_some_and(|db_err| db_err.code() == &SqlState::INVALID_SQL_STATEMENT_NAME)
}

/// True for either way a cached statement handle can go bad.
pub fn is_stale_statement(err: &Error) -> bool {
    is_stale_cached_plan(err) || is_missing_prepared_statement(err)
}

/// Evict every cached statement on every connection of `pool`. A schema
/// change invalidates the same statement on all warm connections, so a
/// per-connection eviction would leave the next retry to trip over another
/// stale copy.
pub fn invalidate_pool_statement_caches(pool: Option<&Pool>) {
    if let Some(pool) = pool {
        pool.manager().statement_caches.clear();
    }
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

/// Runs a cached statement, and on a stale handle evicts it pool-wide and
/// re-runs once on the same connection (outside a transaction only).
macro_rules! run_cached {
    ($self:ident, $method:ident, $sql:expr, $params:expr) => {{
        let statement = $self.prepare_cached($sql).await?;
        match $self.$method(&statement, $params).await {
            Err(err) if is_stale_statement(&err) => {
                $self.evict_stale($sql);
                if !$self.can_retry_in_place() {
                    return Err(err);
                }
                let statement = $self.prepare_cached($sql).await?;
                $self.$method(&statement, $params).await
            }
            result => result,
        }
    }};
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
                run_cached!(self, query, sql, params)
            }

            async fn cached_query_opt(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Option<Row>, Error> {
                run_cached!(self, query_opt, sql, params)
            }

            async fn cached_query_one(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Row, Error> {
                run_cached!(self, query_one, sql, params)
            }

            async fn cached_execute(
                &self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<u64, Error> {
                run_cached!(self, execute, sql, params)
            }
        }
    };
}

trait StaleStatementCleanup {
    /// Drop stale statements after the server rejected `sql`.
    fn evict_stale(&self, sql: &str);
    /// Whether the statement can be re-prepared and re-run on this
    /// connection right away.
    fn can_retry_in_place(&self) -> bool;
}

impl StaleStatementCleanup for Client {
    fn evict_stale(&self, _sql: &str) {
        tracing::warn!("Dropping cached prepared statements after a schema change; retrying");
        match Client::pool(self) {
            Some(pool) => invalidate_pool_statement_caches(Some(&pool)),
            None => self.statement_cache.clear(),
        }
    }

    fn can_retry_in_place(&self) -> bool {
        true
    }
}

impl StaleStatementCleanup for Transaction<'_> {
    fn evict_stale(&self, _sql: &str) {
        tracing::warn!("Dropping cached prepared statements after a schema change; retrying");
        // The pool is not reachable from a transaction; callers that hold
        // the client evict pool-wide through `invalidate_pool_statement_caches`.
        self.statement_cache.clear();
    }

    fn can_retry_in_place(&self) -> bool {
        // The transaction is aborted after the error; the surrounding
        // `retry_db!` restarts it with a fresh statement.
        false
    }
}

impl_cached_statements!(Client);
impl_cached_statements!(Transaction<'_>);
