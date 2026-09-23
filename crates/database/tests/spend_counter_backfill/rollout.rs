//! Mixed-fleet rollout: readers stay correct before reconciliation, and
//! drift left by writers that skipped the counters can be found and repaired.
use super::coverage::run_cli;
use super::*;
use database::repositories::{ApiKeyRepository, PgAnalyticsRepository};
use database::spend_counters_backfill::spend_counter_readiness;
use database::{SpendBackfillOutcome, SpendDrift};
use services::admin::AnalyticsRepository;

const USD: i64 = 1_000_000_000;

async fn set_ready(pool: &DbPool, organization_id: Uuid, ready: bool) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE organization_balance
             SET spend_counters_ready_at = CASE WHEN $2 THEN NOW() ELSE NULL END
             WHERE organization_id = $1",
            &[&organization_id, &ready],
        )
        .await?;
    Ok(())
}

async fn counted(
    pool: &DbPool,
    organization_id: Uuid,
    api_key_id: Uuid,
) -> anyhow::Result<(i64, i64)> {
    let client = pool.get().await?;
    let organization: i64 = client
        .query_one(
            "SELECT inference_spent FROM organization_balance WHERE organization_id = $1",
            &[&organization_id],
        )
        .await?
        .get(0);
    let key: i64 = client
        .query_one(
            "SELECT COALESCE((SELECT inference_spent FROM api_key_spend WHERE api_key_id = $1), 0)",
            &[&api_key_id],
        )
        .await?
        .get(0);
    Ok((organization, key))
}

async fn ready_at(
    pool: &DbPool,
    organization_id: Uuid,
) -> anyhow::Result<Option<chrono::DateTime<Utc>>> {
    let client = pool.get().await?;
    Ok(client
        .query_one(
            "SELECT spend_counters_ready_at FROM organization_balance WHERE organization_id = $1",
            &[&organization_id],
        )
        .await?
        .get(0))
}

async fn key_usages(pool: &DbPool, fixture: &Fixture) -> anyhow::Result<Vec<(Uuid, i64)>> {
    let mut keys: Vec<(Uuid, i64)> = ApiKeyRepository::new(pool.clone())
        .list_by_workspace_paginated(fixture.workspace_id, 100, 0, None, None)
        .await?
        .into_iter()
        .map(|key| (key.id, key.usage))
        .collect();
    keys.sort();
    Ok(keys)
}

/// A ready organization whose counters trail raw history by 100 USD of
/// inference, as left by a writer that inserted usage without counting it.
async fn drifted_ready_organization(pool: &DbPool) -> anyhow::Result<Fixture> {
    let fixture = fixture(pool).await?;
    OrganizationUsageRepository::new(pool.clone())
        .record_usage(inference_request(&fixture, 5 * USD))
        .await?;
    insert_inference(pool, &fixture, fixture.mixed_key, 100 * USD).await?;
    assert!(ready_at(pool, fixture.organization_id).await?.is_some());
    assert_eq!(
        counted(pool, fixture.organization_id, fixture.mixed_key).await?,
        (5 * USD, 5 * USD)
    );
    Ok(fixture)
}

#[tokio::test]
async fn unreconciled_readers_fall_back_to_raw_history() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let pool = database.pool.clone();
        let fixture = fixture(&pool).await?;
        // History written before counters existed: raw rows, zero counters.
        insert_inference(&pool, &fixture, fixture.mixed_key, 100 * USD).await?;
        insert_service(&pool, &fixture, fixture.mixed_key, 40 * USD).await?;
        insert_service(&pool, &fixture, fixture.service_key, 60 * USD).await?;
        set_ready(&pool, fixture.organization_id, false).await?;

        let usage = OrganizationUsageRepository::new(pool.clone());
        assert_eq!(usage.get_api_key_spend(fixture.mixed_key).await?, 100 * USD);
        assert_eq!(usage.get_api_key_spend(fixture.service_key).await?, 0);
        let mut expected = vec![
            (fixture.mixed_key, 140 * USD),
            (fixture.service_key, 60 * USD),
            (fixture.empty_key, 0),
        ];
        expected.sort();
        assert_eq!(key_usages(&pool, &fixture).await?, expected);
        let summary = PgAnalyticsRepository::new(pool.clone())
            .get_billing_summary()
            .await?;
        assert_eq!(summary.inference_consumed_usd, 100.0);
        assert_eq!(summary.service_consumed_usd, 100.0);

        // Once marked ready, the same readers trust the (still zero) counters.
        set_ready(&pool, fixture.organization_id, true).await?;
        assert_eq!(usage.get_api_key_spend(fixture.mixed_key).await?, 0);
        assert!(key_usages(&pool, &fixture)
            .await?
            .iter()
            .all(|(_, usage)| *usage == 0));
        let summary = PgAnalyticsRepository::new(pool.clone())
            .get_billing_summary()
            .await?;
        assert_eq!(summary.inference_consumed_usd, 0.0);
        assert_eq!(summary.service_consumed_usd, 0.0);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn readiness_reports_counts_instead_of_failing() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let fixture = fixture(&database.pool).await?;
        assert!(spend_counter_readiness(&database.pool).await?.is_ready());
        set_ready(&database.pool, fixture.organization_id, false).await?;
        let readiness = spend_counter_readiness(&database.pool).await?;
        assert!(!readiness.is_ready());
        assert_eq!(readiness.missing_balances, 0);
        assert_eq!(readiness.incomplete, 1);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn include_ready_reconciliation_repairs_drift() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let pool = database.pool.clone();
        let fixture = drifted_ready_organization(&pool).await?;
        let timeout = Duration::from_secs(30);

        // The incomplete-only path cannot see drift on a ready organization.
        assert!(
            PreparedSpendBackfill::prepare(&pool, fixture.organization_id, timeout)
                .await?
                .is_none()
        );

        let before = ready_at(&pool, fixture.organization_id).await?;
        let prepared =
            PreparedSpendBackfill::prepare_including_ready(&pool, fixture.organization_id, timeout)
                .await?;
        assert_eq!(
            prepared.drift(),
            SpendDrift {
                inference_spent: 100 * USD,
                service_spent: 0,
                key_count: 1,
            }
        );
        assert_eq!(
            prepared.apply().await?,
            SpendBackfillOutcome::Applied { key_count: 1 }
        );
        assert_eq!(
            counted(&pool, fixture.organization_id, fixture.mixed_key).await?,
            (105 * USD, 105 * USD)
        );
        assert_eq!(
            OrganizationUsageRepository::new(pool.clone())
                .get_api_key_spend(fixture.mixed_key)
                .await?,
            105 * USD
        );
        assert!(ready_at(&pool, fixture.organization_id).await? > before);

        let again =
            PreparedSpendBackfill::prepare_including_ready(&pool, fixture.organization_id, timeout)
                .await?;
        assert_eq!(again.drift(), SpendDrift::default());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn concurrent_include_ready_reconciliations_apply_once() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let pool = database.pool.clone();
        let fixture = drifted_ready_organization(&pool).await?;
        let timeout = Duration::from_secs(30);
        let first =
            PreparedSpendBackfill::prepare_including_ready(&pool, fixture.organization_id, timeout)
                .await?;
        let second =
            PreparedSpendBackfill::prepare_including_ready(&pool, fixture.organization_id, timeout)
                .await?;

        let (first, second) = tokio::join!(first.apply(), second.apply());
        let outcomes = [first?, second?];
        assert!(outcomes.contains(&SpendBackfillOutcome::Applied { key_count: 1 }));
        assert!(outcomes.contains(&SpendBackfillOutcome::AlreadyComplete));
        assert_eq!(
            counted(&pool, fixture.organization_id, fixture.mixed_key).await?,
            (105 * USD, 105 * USD)
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cli_dry_run_reports_drift_without_applying() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let pool = database.pool.clone();
        let fixture = drifted_ready_organization(&pool).await?;
        let verify = ["--include-ready", "--dry-run"];

        let output = run_cli(&database.database_name, &verify).await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "drift must fail a dry run");
        assert!(stderr.contains("drift"), "dry-run stderr: {stderr}");
        assert!(
            stdout.contains(&fixture.organization_id.to_string()),
            "dry-run stdout did not identify the organization: {stdout}"
        );
        assert_eq!(
            counted(&pool, fixture.organization_id, fixture.mixed_key).await?,
            (5 * USD, 5 * USD),
            "a dry run must not apply corrections"
        );

        let output = run_cli(&database.database_name, &["--include-ready"]).await?;
        assert!(
            output.status.success(),
            "repair failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            counted(&pool, fixture.organization_id, fixture.mixed_key).await?,
            (105 * USD, 105 * USD)
        );

        let output = run_cli(&database.database_name, &verify).await?;
        assert!(
            output.status.success(),
            "verification after repair failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    })
    .await
}

/// V0081__add_spend_counters introduces the counters and the readiness marker;
/// keep this at the schema version immediately before it.
const LAST_SCHEMA_BEFORE_SPEND_COUNTERS: i32 = 80;

/// The production deploy: a database on the last schema before spend counters
/// takes the full current migration set, with no reconciliation yet.
#[tokio::test]
async fn deploy_from_pre_counter_schema_serves_raw_then_counters() -> anyhow::Result<()> {
    run_backfill_test_at(
        Some(LAST_SCHEMA_BEFORE_SPEND_COUNTERS),
        |database| async move {
            let pool = database.pool.clone();
            let fixture = fixture(&pool).await?;
            insert_inference(&pool, &fixture, fixture.mixed_key, 100 * USD).await?;
            insert_service(&pool, &fixture, fixture.mixed_key, 40 * USD).await?;
            insert_service(&pool, &fixture, fixture.service_key, 60 * USD).await?;

            migrations::run(&pool).await?;
            let readiness = spend_counter_readiness(&pool).await?;
            assert_eq!((readiness.missing_balances, readiness.incomplete), (0, 1));

            // Usage posted by a new writer before reconciliation is counted and raw.
            OrganizationUsageRepository::new(pool.clone())
                .record_usage(inference_request(&fixture, 5 * USD))
                .await?;
            let mut expected = vec![
                (fixture.mixed_key, 145 * USD),
                (fixture.service_key, 60 * USD),
                (fixture.empty_key, 0),
            ];
            expected.sort();
            let usage = OrganizationUsageRepository::new(pool.clone());
            let analytics = PgAnalyticsRepository::new(pool.clone());
            assert_eq!(usage.get_api_key_spend(fixture.mixed_key).await?, 105 * USD);
            assert_eq!(key_usages(&pool, &fixture).await?, expected);
            let summary = analytics.get_billing_summary().await?;
            assert_eq!(
                (summary.inference_consumed_usd, summary.service_consumed_usd),
                (105.0, 100.0)
            );

            let output = run_cli(&database.database_name, &[]).await?;
            assert!(
                output.status.success(),
                "backfill failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(spend_counter_readiness(&pool).await?.is_ready());

            // Same answers, now from the counters.
            assert_eq!(usage.get_api_key_spend(fixture.mixed_key).await?, 105 * USD);
            assert_eq!(key_usages(&pool, &fixture).await?, expected);
            let summary = analytics.get_billing_summary().await?;
            assert_eq!(
                (summary.inference_consumed_usd, summary.service_consumed_usd),
                (105.0, 100.0)
            );
            assert_eq!(
                counted(&pool, fixture.organization_id, fixture.mixed_key).await?,
                (105 * USD, 105 * USD)
            );
            let verify =
                run_cli(&database.database_name, &["--include-ready", "--dry-run"]).await?;
            assert!(
                verify.status.success(),
                "post-backfill verification found drift: {}",
                String::from_utf8_lossy(&verify.stderr)
            );
            Ok(())
        },
    )
    .await
}

/// A balance row is created with its organization (V0004), so a missing one is
/// an anomaly, not an empty organization: raw history may still exist.
#[tokio::test]
async fn readers_with_missing_organization_balance_row_use_raw_history() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let pool = database.pool.clone();
        let fixture = fixture(&pool).await?;
        insert_inference(&pool, &fixture, fixture.mixed_key, 100 * USD).await?;
        insert_service(&pool, &fixture, fixture.service_key, 60 * USD).await?;
        pool.get()
            .await?
            .execute(
                "DELETE FROM organization_balance WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?;

        let usage = OrganizationUsageRepository::new(pool.clone());
        assert_eq!(usage.get_api_key_spend(fixture.mixed_key).await?, 100 * USD);
        let mut expected = vec![
            (fixture.mixed_key, 100 * USD),
            (fixture.service_key, 60 * USD),
            (fixture.empty_key, 0),
        ];
        expected.sort();
        assert_eq!(key_usages(&pool, &fixture).await?, expected);
        let summary = PgAnalyticsRepository::new(pool.clone())
            .get_billing_summary()
            .await?;
        assert_eq!(
            (summary.inference_consumed_usd, summary.service_consumed_usd),
            (100.0, 60.0)
        );
        Ok(())
    })
    .await
}

/// An unreconciled organization with no usage has zero drift but is not done;
/// a dry run used as the rollout gate must not pass while it remains.
#[tokio::test]
async fn cli_dry_run_fails_while_organizations_remain_unreconciled() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let fixture = fixture(&database.pool).await?;
        set_ready(&database.pool, fixture.organization_id, false).await?;

        let output = run_cli(&database.database_name, &["--include-ready", "--dry-run"]).await?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "unreconciled organization must fail a dry run"
        );
        assert!(
            stderr.contains("not reconciled"),
            "dry-run stderr: {stderr}"
        );
        assert!(ready_at(&database.pool, fixture.organization_id)
            .await?
            .is_none());
        Ok(())
    })
    .await
}
