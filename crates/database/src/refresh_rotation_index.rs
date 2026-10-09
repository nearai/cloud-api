use crate::DbPool;
use anyhow::{bail, Context, Result};

/// The predecessor lookup is part of every refresh-token validation query.
/// Without this index, PostgreSQL may scan the entire refresh_tokens table for the
/// OR branch even when the current-token hash is indexed.
pub async fn ensure_refresh_rotation_index(pool: &DbPool) -> Result<()> {
    let client = pool
        .get()
        .await
        .context("Failed to get database connection for refresh index check")?;
    let ready: bool = client
        .query_one(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_namespace AS namespace
                JOIN pg_class AS table_class
                  ON table_class.relnamespace = namespace.oid
                 AND table_class.relname = 'refresh_tokens'
                 AND table_class.relkind IN ('r', 'p')
                JOIN pg_index AS index_state
                  ON index_state.indrelid = table_class.oid
                JOIN pg_class AS index_class
                  ON index_class.oid = index_state.indexrelid
                 AND index_class.relnamespace = namespace.oid
                 AND index_class.relname = 'idx_refresh_tokens_previous_hash'
                 AND index_class.relkind = 'i'
                WHERE namespace.nspname = current_schema()
                  AND index_state.indisvalid
                  AND index_state.indisready
            )
            "#,
            &[],
        )
        .await
        .context("Failed to verify refresh rotation index")?
        .get(0);
    if !ready {
        bail!("refresh rotation index idx_refresh_tokens_previous_hash is missing, invalid, or unready");
    }
    Ok(())
}
