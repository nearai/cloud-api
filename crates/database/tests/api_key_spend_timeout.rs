//! The API key spend read is bounded by a transaction-local `statement_timeout`.
//! A cancelled read maps to `QueryTimeout`, runs once (no retry), and leaves the
//! pooled connection's own setting untouched.

use std::time::Duration;

use database::repositories::OrganizationUsageRepository;
use database::DbPool;
use deadpool::Runtime;
use deadpool_postgres::Config;
use services::common::RepositoryError;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[path = "support/schema.rs"]
mod schema;

async fn single_connection_pool() -> anyhow::Result<DbPool> {
    schema::ensure_schema().await?;
    let mut config = Config::new();
    config.host = Some(std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_string()));
    config.port = Some(
        std::env::var("PGPORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(5432),
    );
    config.dbname =
        Some(std::env::var("PGDATABASE").unwrap_or_else(|_| "platform_api".to_string()));
    config.user = Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_string()));
    config.password = Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".to_string()));
    let pool = config.create_pool(Some(Runtime::Tokio1), NoTls)?;
    pool.resize(1);
    Ok(pool.into())
}

async fn session_timeout(pool: &DbPool) -> anyhow::Result<String> {
    let client = pool.get().await?;
    Ok(client
        .query_one("SHOW statement_timeout", &[])
        .await?
        .get(0))
}

#[tokio::test]
async fn api_key_spend_timeout_is_not_retried_and_does_not_leak() -> anyhow::Result<()> {
    let pool = single_connection_pool().await?;
    let repo = OrganizationUsageRepository::new(pool.clone());
    let original = session_timeout(&pool).await?;

    // Session-local stand-in for the usage table: each evaluation counts itself,
    // then sleeps. The counter survives a cancelled statement, so it shows how
    // many times the read was attempted.
    let client = pool.get().await?;
    client
        .batch_execute(
            r#"
            CREATE TEMP SEQUENCE spend_attempts;
            CREATE FUNCTION pg_temp.slow_cost() RETURNS bigint LANGUAGE plpgsql AS $$
            BEGIN
                PERFORM nextval('pg_temp.spend_attempts');
                PERFORM pg_sleep(5);
                RETURN 0;
            END $$;
            CREATE TEMP VIEW organization_usage_log AS
                SELECT NULL::uuid AS api_key_id, pg_temp.slow_cost() AS total_cost;
            "#,
        )
        .await?;
    drop(client);

    let error = repo
        .get_api_key_spend_within(Uuid::new_v4(), Duration::from_millis(200))
        .await
        .expect_err("the sum is cancelled by the timeout");
    assert!(
        matches!(
            error.downcast_ref::<RepositoryError>(),
            Some(RepositoryError::QueryTimeout)
        ),
        "unexpected error kind"
    );

    let client = pool.get().await?;
    let attempts: i64 = client
        .query_one("SELECT last_value FROM spend_attempts", &[])
        .await?
        .get(0);
    assert_eq!(attempts, 1, "a cancelled read must not be retried");
    client
        .batch_execute("DROP VIEW organization_usage_log; DROP FUNCTION pg_temp.slow_cost();")
        .await?;
    drop(client);

    assert_eq!(session_timeout(&pool).await?, original);

    // The real table answers, and the setting still does not outlive the call.
    assert_eq!(repo.get_api_key_spend(Uuid::new_v4()).await?, 0);
    assert_eq!(session_timeout(&pool).await?, original);
    Ok(())
}
