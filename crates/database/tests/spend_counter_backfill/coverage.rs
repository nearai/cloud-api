use super::*;
use database::spend_counters_backfill::incomplete_organizations;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

fn stable_id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

async fn insert_empty_organization(pool: &DbPool, organization_id: Uuid) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO organizations (id, name, created_at, updated_at)
             VALUES ($1, $2, NOW(), NOW())",
            &[
                &organization_id,
                &format!("backfill-test-{organization_id}"),
            ],
        )
        .await?;
    Ok(())
}

async fn set_incomplete(pool: &DbPool, organization_id: Uuid) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE organization_balance
             SET spend_counters_ready_at = NULL
             WHERE organization_id = $1",
            &[&organization_id],
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn incomplete_organizations_orders_and_pages_ready_incomplete_and_missing_rows(
) -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let ready = stable_id(1);
        let incomplete = stable_id(2);
        let missing = stable_id(3);
        for organization_id in [ready, incomplete, missing] {
            insert_empty_organization(&database.pool, organization_id).await?;
        }
        set_incomplete(&database.pool, incomplete).await?;
        let client = database.pool.get().await?;
        client
            .execute(
                "DELETE FROM organization_balance WHERE organization_id = $1",
                &[&missing],
            )
            .await?;
        drop(client);

        let first = incomplete_organizations(&database.pool, None, 1).await?;
        assert_eq!(first, vec![incomplete]);
        let second = incomplete_organizations(&database.pool, first.last().copied(), 1).await?;
        assert_eq!(second, vec![missing]);
        let third = incomplete_organizations(&database.pool, second.last().copied(), 1).await?;
        assert!(third.is_empty());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn prepare_fails_for_organization_without_balance_row() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let fixture = fixture(&database.pool).await?;
        let client = database.pool.get().await?;
        client
            .execute(
                "DELETE FROM organization_balance WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?;
        drop(client);

        let error = PreparedSpendBackfill::prepare(
            &database.pool,
            fixture.organization_id,
            Duration::from_secs(30),
        )
        .await
        .expect_err("missing balance row must fail preparation");
        assert_eq!(
            error.to_string(),
            format!(
                "organization {} has no organization_balance row",
                fixture.organization_id
            )
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn prepare_rejects_zero_submillisecond_and_oversized_timeouts() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let organization_id = Uuid::new_v4();
        let timeouts = [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_millis(i32::MAX as u64 + 1),
        ];
        for statement_timeout in timeouts {
            let error =
                PreparedSpendBackfill::prepare(&database.pool, organization_id, statement_timeout)
                    .await
                    .expect_err("invalid timeout must fail before acquiring a connection");
            assert!(error.to_string().contains("between 1ms and 2147483647ms"));
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn apply_times_out_when_accounting_lock_is_held() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let fixture = fixture(&database.pool).await?;
        set_incomplete(&database.pool, fixture.organization_id).await?;
        let prepared = PreparedSpendBackfill::prepare(
            &database.pool,
            fixture.organization_id,
            Duration::from_secs(30),
        )
        .await?
        .expect("fixture organization is incomplete");

        let mut lock_client = database.pool.get().await?;
        let lock_transaction = lock_client.transaction().await?;
        lock_transaction
            .query_one(
                "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
                &[&fixture.organization_id],
            )
            .await?;

        let started = Instant::now();
        let result = timeout(Duration::from_secs(15), prepared.apply())
            .await
            .expect("apply must finish within the outer 15 second bound");
        let elapsed = started.elapsed();
        let error = result.expect_err("the held accounting lock must time out");
        assert!(elapsed >= Duration::from_secs(4), "apply returned too early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(15), "apply exceeded outer bound: {elapsed:?}");
        // Both deadlines are 5s. map_db_error maps statement cancellation (57014)
        // to QueryTimeout, while a lock timeout retains SQLSTATE 55P03.
        let repository_error = error
            .downcast_ref::<services::common::RepositoryError>()
            .expect("accounting lock failures retain their repository error type");
        match repository_error {
            services::common::RepositoryError::QueryTimeout => {},
            services::common::RepositoryError::DatabaseError(cause) => assert!(
                cause.to_string().starts_with("Database error (55P03): "),
                "expected lock timeout SQLSTATE 55P03, got: {cause}"
            ),
            other => panic!("unexpected accounting lock failure: {other}"),
        }

        let client = database.pool.get().await?;
        let ready_at = client
            .query_one(
                "SELECT spend_counters_ready_at FROM organization_balance WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?
            .get::<_, Option<chrono::DateTime<Utc>>>(0);
        assert!(ready_at.is_none(), "failed apply must leave readiness unchanged");
        drop(client);
        lock_transaction.rollback().await?;
        Ok(())
    })
    .await
}

fn cli_command(database_name: &str) -> Command {
    let config = pool_config(None);
    let host = config.host.unwrap_or_else(|| "localhost".to_string());
    let port = config.port.unwrap_or(5432).to_string();
    let user = config.user.unwrap_or_else(|| "postgres".to_string());
    let password = config.password.unwrap_or_else(|| "postgres".to_string());
    let mut command = Command::new(env!("CARGO_BIN_EXE_backfill-spend-counters"));
    command
        .env("DATABASE_CONNECTION_MODE", "patroni")
        .env("POSTGRES_PRIMARY_APP_ID", "postgres-test")
        .env("GATEWAY_SUBDOMAIN", "localhost")
        .env("DATABASE_HOST", host)
        .env("DATABASE_PORT", port)
        .env("DATABASE_NAME", database_name)
        .env("DATABASE_USERNAME", user)
        .env("DATABASE_PASSWORD", password)
        .env("DATABASE_TLS_ENABLED", "false")
        .env("DATABASE_MAX_CONNECTIONS", "4")
        .env("DATABASE_REFRESH_INTERVAL", "30")
        .stdin(std::process::Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub(super) async fn run_cli(database_name: &str, args: &[&str]) -> anyhow::Result<Output> {
    let mut command = cli_command(database_name);
    command.args(args).kill_on_drop(true);
    Ok(timeout(Duration::from_secs(60), command.output()).await??)
}

#[tokio::test]
async fn cli_reconciles_more_than_one_batch_of_empty_organizations() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        for value in 1..=101 {
            let organization_id = stable_id(10_000 + value);
            insert_empty_organization(&database.pool, organization_id).await?;
            set_incomplete(&database.pool, organization_id).await?;
        }

        let output = run_cli(&database.database_name, &[]).await?;
        assert!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let client = database.pool.get().await?;
        let ready_count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM organization_balance
                 WHERE spend_counters_ready_at IS NOT NULL",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(ready_count, 101);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cli_stops_on_anomaly_and_preserves_fail_fast_exit() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let anomalous = stable_id(20_000);
        let remaining = stable_id(20_001);
        insert_empty_organization(&database.pool, anomalous).await?;
        insert_empty_organization(&database.pool, remaining).await?;
        set_incomplete(&database.pool, remaining).await?;
        let client = database.pool.get().await?;
        client
            .execute(
                "DELETE FROM organization_balance WHERE organization_id = $1",
                &[&anomalous],
            )
            .await?;
        drop(client);

        let output = run_cli(&database.database_name, &[]).await?;
        assert!(!output.status.success(), "anomalous organization must fail the CLI");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("no organization_balance row"),
            "CLI stderr did not identify the anomaly: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let client = database.pool.get().await?;
        let ready_at = client
            .query_one(
                "SELECT spend_counters_ready_at FROM organization_balance WHERE organization_id = $1",
                &[&remaining],
            )
            .await?
            .get::<_, Option<chrono::DateTime<Utc>>>(0);
        assert!(ready_at.is_none(), "fail-fast CLI must not reconcile later organizations");
        Ok(())
    })
    .await
}
