use crate::pool::DbPool;
use anyhow::{Context, Result};
use refinery::embed_migrations;
use tracing::info;

// Embed migrations from the migrations folder
embed_migrations!("src/migrations/sql");

/// Run database migrations
pub async fn run(pool: &DbPool) -> Result<()> {
    let mut client = pool
        .get()
        .await
        .context("Failed to get database connection for migrations")?;

    let migration_report = migrations::runner()
        .run_async(&mut **client)
        .await
        .context("Failed to run migrations")?;

    for migration in migration_report.applied_migrations() {
        info!("Applied migration: {}", migration.name());
    }

    info!("All migrations completed successfully");
    Ok(())
}
