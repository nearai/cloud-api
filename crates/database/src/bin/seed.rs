use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use database::Database;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (ignore errors if not found)
    let _ = dotenvy::dotenv();

    // Initialize tracing for CLI output
    tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .init();

    info!("Starting database seeding");

    // Load database config from environment
    let db_config = config::DatabaseConfig::from_env()
        .map_err(|e| anyhow!("Failed to load database config: {e}"))?;
    info!("Database configuration loaded");

    // Connect to database
    let database = Database::from_config(&db_config)
        .await
        .context("Failed to connect to database")?;
    info!("Connected to database");

    // Run migrations
    database
        .run_migrations()
        .await
        .context("Failed to run migrations")?;
    info!("Database migrations completed");

    // Run seed scripts
    run_seed_scripts(&database).await?;
    info!("Database seeding completed");
    Ok(())
}

async fn run_seed_scripts(database: &Database) -> Result<()> {
    let seed_dir = PathBuf::from("crates/database/src/seed");

    if !seed_dir.exists() {
        return Err(anyhow!("Seed directory not found: {}", seed_dir.display()));
    }

    // Read all .sql files in the seed directory
    let mut seed_files: Vec<_> = fs::read_dir(&seed_dir)
        .context("Failed to read seed directory")?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "sql") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort files to ensure consistent execution order
    seed_files.sort();

    if seed_files.is_empty() {
        info!("No seed files found, skipping seed scripts");
        return Ok(());
    }

    let pool = database.pool();

    for seed_file in seed_files {
        let file_name = seed_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Read the SQL file
        let sql = fs::read_to_string(&seed_file)
            .context(format!("Failed to read seed file: {}", seed_file.display()))?;

        // Execute the SQL
        let client = pool
            .get()
            .await
            .context("Failed to get database connection")?;

        client
            .batch_execute(&sql)
            .await
            .context(format!("Failed to execute seed script: {file_name}"))?;

        info!("Executed seed script: {}", file_name);
    }

    Ok(())
}
