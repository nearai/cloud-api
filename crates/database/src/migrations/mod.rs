use crate::pool::DbPool;
use anyhow::{Context, Result};
use deadpool_postgres::Client;
use refinery::load_sql_migrations;
use std::time::Duration;
use tracing::info;

// "NEARINDX" as an i64. PostgreSQL permits only one concurrent index build on
// a table at a time, so application replicas must serialize this bootstrap.
const REFRESH_ROTATION_INDEX_LOCK_KEY: i64 = 0x4e45_4152_494e_4458;
const REFRESH_ROTATION_INDEX: &str = "idx_refresh_tokens_previous_hash";

/// Holds the connection that serializes the out-of-transaction index build.
struct RefreshRotationIndexLock {
    client: Option<Client>,
}

impl RefreshRotationIndexLock {
    async fn acquire(client: Client) -> Result<Self> {
        let mut guard = Self {
            client: Some(client),
        };

        loop {
            let acquired: bool = guard
                .client()
                .query_one(
                    "SELECT pg_try_advisory_lock($1)",
                    &[&REFRESH_ROTATION_INDEX_LOCK_KEY],
                )
                .await
                .context("Failed to acquire refresh rotation index lock")?
                .get(0);
            if acquired {
                return Ok(guard);
            }

            // Do not wait inside pg_advisory_lock: that open statement holds a
            // virtual transaction which CREATE INDEX CONCURRENTLY may need to
            // wait for, creating a deadlock with the lock holder.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn client(&mut self) -> &mut tokio_postgres::Client {
        self.client
            .as_mut()
            .expect("refresh rotation index lock must own its connection")
    }

    async fn release(mut self) -> Result<()> {
        let row = self
            .client()
            .query_one(
                "SELECT pg_advisory_unlock($1)",
                &[&REFRESH_ROTATION_INDEX_LOCK_KEY],
            )
            .await
            .context("Failed to release refresh rotation index lock")?;
        let unlocked: bool = row.get(0);
        anyhow::ensure!(unlocked, "Refresh rotation index lock was not held");

        // The session no longer owns the lock and can safely return to the pool.
        drop(self.client.take());
        Ok(())
    }
}

impl Drop for RefreshRotationIndexLock {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            // Cancellation or an unlock failure must not return a session that
            // may still own the advisory lock to the pool. Closing the session
            // releases the lock.
            drop(Client::take(client));
        }
    }
}

async fn refresh_rotation_index_is_ready(client: &tokio_postgres::Client) -> Result<Option<bool>> {
    let row = client
        .query_opt(
            r#"
            SELECT index_state.indisvalid AND index_state.indisready
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
             AND index_class.relname = $1
             AND index_class.relkind = 'i'
            WHERE namespace.nspname = current_schema()
            "#,
            &[&REFRESH_ROTATION_INDEX],
        )
        .await
        .context("Failed to inspect refresh rotation index")?;
    Ok(row.map(|row| row.get(0)))
}

/// Build the refresh-token predecessor index without blocking authentication
/// writes. This runs outside Refinery's transaction and is serialized separately
/// because PostgreSQL allows only one concurrent index build per table.
async fn ensure_refresh_rotation_index(client: &tokio_postgres::Client) -> Result<()> {
    match refresh_rotation_index_is_ready(client).await? {
        Some(true) => return Ok(()),
        Some(false) => {
            // PostgreSQL can leave an INVALID index behind when a concurrent
            // build is interrupted. IF NOT EXISTS would otherwise skip it.
            client
                .batch_execute("DROP INDEX CONCURRENTLY IF EXISTS idx_refresh_tokens_previous_hash")
                .await
                .context("Failed to drop invalid refresh rotation index")?;
        }
        None => {}
    }

    client
        .batch_execute(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_refresh_tokens_previous_hash \
             ON refresh_tokens(previous_token_hash) \
             WHERE previous_token_hash IS NOT NULL",
        )
        .await
        .context("Failed to create refresh rotation index concurrently")?;

    anyhow::ensure!(
        refresh_rotation_index_is_ready(client).await? == Some(true),
        "Refresh rotation index is missing, invalid, or unready after creation"
    );
    info!(
        index = REFRESH_ROTATION_INDEX,
        "Refresh rotation index ready"
    );
    Ok(())
}

/// Run database migrations
pub async fn run(pool: &DbPool) -> Result<()> {
    let mut client = pool
        .get()
        .await
        .context("Failed to get database connection for migrations")?;

    // Load the migration SQL files from the migrations/sql folder
    // Priority: 1) DATABASE_MIGRATIONS_PATH env var, 2) relative path from current dir, 3) compile-time path
    let env_path = std::env::var("DATABASE_MIGRATIONS_PATH")
        .ok()
        .map(std::path::PathBuf::from);
    let relative_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("crates/database/src/migrations/sql");
    let compile_time_path =
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/migrations/sql"));

    let candidate_paths: Vec<_> = env_path
        .iter()
        .chain([&relative_path, &compile_time_path])
        .cloned()
        .collect();

    let migrations_path = candidate_paths
        .iter()
        .find(|path| path.exists())
        .ok_or_else(|| {
            let paths_str = candidate_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("Migrations folder not found. Checked paths: {paths_str}")
        })?;

    let migrations = load_sql_migrations(migrations_path)
        .with_context(|| format!("Failed to load migrations from {migrations_path:?}"))?;

    let migration_report = refinery::Runner::new(&migrations)
        .run_async(&mut **client)
        .await
        .context("Failed to run migrations")?;

    // Regular migrations retain Refinery's existing behavior. Only replicas
    // that observe a missing or invalid out-of-transaction index contend on
    // the dedicated bootstrap lock; the normal startup path takes no lock.
    if refresh_rotation_index_is_ready(&client).await? != Some(true) {
        let mut index_lock = RefreshRotationIndexLock::acquire(client).await?;
        let index_result = ensure_refresh_rotation_index(index_lock.client()).await;
        let unlock_result = index_lock.release().await;
        match (index_result, unlock_result) {
            (Ok(()), Ok(())) => {}
            (Err(index_error), Ok(())) => return Err(index_error),
            (Ok(()), Err(unlock_error)) => return Err(unlock_error),
            (Err(index_error), Err(unlock_error)) => {
                return Err(index_error).context(format!(
                    "Failed to prepare refresh rotation index; additionally failed to release its lock: {unlock_error:#}"
                ));
            }
        }
    }

    for migration in migration_report.applied_migrations() {
        info!("Applied migration: {}", migration.name());
    }

    info!("All migrations completed successfully");
    Ok(())
}
