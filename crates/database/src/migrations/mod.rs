use crate::pool::DbPool;
use anyhow::{Context, Result};
use deadpool_postgres::Client;
use refinery::load_sql_migrations;
use tracing::info;

// "NEARMIGR" as an i64. Session-level advisory locks are scoped to a single
// database, so independent databases can still migrate in parallel.
const MIGRATION_LOCK_KEY: i64 = 0x4e45_4152_4d49_4752;

/// Holds the dedicated connection that owns the PostgreSQL advisory lock.
///
/// If this future is cancelled or unlocking fails, discard the connection
/// instead of returning a session that may still own the lock to the pool.
struct MigrationLock {
    client: Option<Client>,
    safe_to_reuse: bool,
}

impl MigrationLock {
    async fn acquire(pool: &DbPool) -> Result<Self> {
        let client = pool
            .get()
            .await
            .context("Failed to get database connection for migrations")?;
        let mut guard = Self {
            client: Some(client),
            safe_to_reuse: false,
        };

        guard
            .client()
            .query_one("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK_KEY])
            .await
            .context("Failed to acquire database migration lock")?;

        Ok(guard)
    }

    fn client(&mut self) -> &mut tokio_postgres::Client {
        self.client
            .as_mut()
            .expect("migration lock must own its connection")
    }

    async fn release(mut self) -> Result<()> {
        let row = self
            .client()
            .query_one("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK_KEY])
            .await
            .context("Failed to release database migration lock")?;
        let unlocked: bool = row.get(0);
        anyhow::ensure!(unlocked, "Database migration lock was not held");
        self.safe_to_reuse = true;
        Ok(())
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        if !self.safe_to_reuse {
            if let Some(client) = self.client.take() {
                // Closing the PostgreSQL session releases any advisory lock it
                // may still own. Do not put this connection back in the pool.
                drop(Client::take(client));
            }
        }
    }
}

/// Run database migrations
pub async fn run(pool: &DbPool) -> Result<()> {
    // Refinery's schema history check and insert are not atomic across runner
    // processes. Serialize the whole operation so concurrent application
    // instances and integration-test binaries cannot apply the same version.
    let mut lock = MigrationLock::acquire(pool).await?;

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

    let migration_result = refinery::Runner::new(&migrations)
        .run_async(lock.client())
        .await;
    let unlock_result = lock.release().await;

    let migration_report = match (migration_result, unlock_result) {
        (Ok(report), Ok(())) => report,
        (Err(migration_error), Ok(())) => {
            return Err(migration_error).context("Failed to run migrations");
        }
        (Ok(_), Err(unlock_error)) => return Err(unlock_error),
        (Err(migration_error), Err(unlock_error)) => {
            return Err(migration_error).context(format!(
                "Failed to run migrations; additionally failed to release migration lock: {unlock_error:#}"
            ));
        }
    };

    for migration in migration_report.applied_migrations() {
        info!("Applied migration: {}", migration.name());
    }

    info!("All migrations completed successfully");
    Ok(())
}
