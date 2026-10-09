//! Concurrent startups must apply each migration once across connection pools.
use database::{migrations, DbPool};
use deadpool::Runtime;
use deadpool_postgres::Config;
use std::sync::Arc;
use tokio::sync::Barrier;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[path = "support/service_usage_reporting_pool.rs"]
mod service_usage_reporting_pool;

#[tokio::test]
async fn concurrent_bootstraps_share_a_database_migration_lock() -> anyhow::Result<()> {
    let admin_pool = service_usage_reporting_pool::test_pool().await?;
    let admin = admin_pool.get().await?;
    // Test-owned identifier contains only ASCII letters, digits and underscores.
    let name = format!("migration_race_{}", Uuid::new_v4().simple());
    admin
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .await?;
    let config = Config {
        host: Some(std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into())),
        port: Some(
            std::env::var("PGPORT")
                .unwrap_or_else(|_| "5432".into())
                .parse()?,
        ),
        user: Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".into())),
        password: Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".into())),
        dbname: Some(name.clone()),
        ..Default::default()
    };
    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..3 {
        // Independent pools model callers in different replicas/test binaries.
        let pool = DbPool::new(config.create_pool(Some(Runtime::Tokio1), NoTls)?);
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            migrations::run(&pool).await
        }));
    }
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await?);
    }
    let pool = DbPool::new(config.create_pool(Some(Runtime::Tokio1), NoTls)?);
    let client = pool.get().await?;
    let latest: i32 = client
        .query_one(
            "SELECT count(*)::integer FROM refinery_schema_history WHERE version=88",
            &[],
        )
        .await?
        .get(0);
    drop(client);
    drop(pool);
    admin
        .batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .await?;
    for result in results {
        result?;
    }
    assert_eq!(latest, 1);
    Ok(())
}
