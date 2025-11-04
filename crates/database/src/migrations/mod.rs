use crate::pool::DbPool;
use anyhow::{Context, Result};
use refinery::load_sql_migrations;
use tracing::info;

/// Run database migrations
pub async fn run(pool: &DbPool) -> Result<()> {
    let mut client = pool
        .get()
        .await
        .context("Failed to get database connection for migrations")?;

    // Load the migration SQL files from the migrations/sql folder
    // Use runtime path resolution relative to current working directory
    let migrations_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("crates/database/src/migrations/sql");

    let migrations = load_sql_migrations(&migrations_path).context(format!(
        "Failed to load migrations from {migrations_path:?}"
    ))?;

    let migration_report = refinery::Runner::new(&migrations)
        .run_async(&mut **client)
        .await
        .context("Failed to run migrations")?;

    for migration in migration_report.applied_migrations() {
        info!("Applied migration: {}", migration.name());
    }

    info!("All migrations completed successfully");
    Ok(())
}
