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

    for migration in migration_report.applied_migrations() {
        info!("Applied migration: {}", migration.name());
    }

    info!("All migrations completed successfully");
    Ok(())
}
