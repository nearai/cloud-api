//! usage_hourly repository: recompute, day parity, progress. Parallel-safe tests use a
//! UUID-scoped fixture org and random far-past hours; recompute(wait=true) serializes.

use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
use chrono::{DateTime, Duration, TimeZone, Utc};
use database::repositories::UsageHourlyRepositoryImpl;
use services::usage::ports::UsageHourlyRepository;

fn random_past_hour() -> DateTime<Utc> {
    let hours = (uuid::Uuid::new_v4().as_u128() % (20 * 365 * 24)) as i64;
    Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).unwrap() + Duration::hours(hours)
}

#[allow(clippy::too_many_arguments)]
async fn insert_raw(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
    created_at: DateTime<Utc>,
    total_cost: i64,
    tokens: i32,
    ttft_ms: Option<i32>,
    stop_reason: Option<&str>,
    served_provider_type: Option<&str>,
) {
    let client = f.database.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO organization_usage_log (
                organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, cache_read_tokens,
                input_cost, output_cost, total_cost, request_type, inference_type, created_at,
                ttft_ms, avg_itl_ms, stop_reason, served_provider_type, served_via_fallback)
             VALUES ($1,$2,$3,$4,$5,$6,0,$6,0,$7,0,$7,'chat_completion','chat_completion',$8,
                     $9,NULL,$10,$11,false)",
            &[
                &f.organization_id,
                &f.workspace_id,
                &f.api_key_id,
                &f.model_id,
                &f.model_name,
                &tokens,
                &total_cost,
                &created_at,
                &ttft_ms,
                &stop_reason,
                &served_provider_type,
            ],
        )
        .await
        .unwrap();
}

async fn org_rows(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
) -> Vec<(DateTime<Utc>, Option<String>, i64, i64, i64, i64)> {
    let client = f.database.pool().get().await.unwrap();
    client
        .query(
            "SELECT hour, served_provider_type, request_count, total_cost, error_count, ttft_count
             FROM usage_hourly WHERE organization_id = $1 ORDER BY hour, served_provider_type NULLS FIRST",
            &[&f.organization_id],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect()
}

#[tokio::test]
async fn recompute_aggregates_exactly_by_utc_hour_including_null_dimensions() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let h = random_past_hour();
    // Review Focus 1: boundary instants; Review Focus 2: NULL provider type kept as its own row.
    insert_raw(&f, h, 100, 10, Some(200), None, Some("external")).await;
    insert_raw(
        &f,
        h + Duration::minutes(59) + Duration::milliseconds(59_999),
        50,
        5,
        None,
        Some("timeout"),
        Some("external"),
    )
    .await;
    insert_raw(&f, h + Duration::minutes(30), 7, 1, Some(100), None, None).await;
    insert_raw(
        &f,
        h + Duration::hours(1),
        1,
        1,
        None,
        None,
        Some("external"),
    )
    .await;

    let report = repo
        .recompute(h, h + Duration::hours(2), true)
        .await
        .unwrap()
        .unwrap();
    assert!(report.rows_written >= 3);
    assert_eq!(
        org_rows(&f).await,
        vec![
            (h, None, 1, 7, 0, 1),
            (h, Some("external".into()), 2, 150, 1, 1),
            (h + Duration::hours(1), Some("external".into()), 1, 1, 0, 0),
        ]
    );
}

#[tokio::test]
async fn recompute_is_idempotent_and_picks_up_late_rows() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let h = random_past_hour();
    insert_raw(&f, h, 10, 1, None, None, Some("external")).await;
    repo.recompute(h, h + Duration::hours(1), true)
        .await
        .unwrap();
    repo.recompute(h, h + Duration::hours(1), true)
        .await
        .unwrap();
    assert_eq!(
        org_rows(&f).await,
        vec![(h, Some("external".into()), 1, 10, 0, 0)]
    );

    insert_raw(
        &f,
        h + Duration::minutes(10),
        5,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    repo.recompute(h, h + Duration::hours(1), true)
        .await
        .unwrap();
    assert_eq!(
        org_rows(&f).await,
        vec![(h, Some("external".into()), 2, 15, 0, 0)]
    );
}

#[tokio::test]
async fn recompute_without_wait_returns_none_when_lock_is_held() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let mut holder = f.database.pool().get().await.unwrap();
    let tx = holder.transaction().await.unwrap();
    tx.execute(
        "SELECT pg_advisory_xact_lock($1)",
        &[&database::repositories::usage_hourly::USAGE_HOURLY_LOCK_KEY],
    )
    .await
    .unwrap();

    let h = random_past_hour();
    assert!(repo
        .recompute(h, h + Duration::hours(1), false)
        .await
        .unwrap()
        .is_none());
    tx.rollback().await.unwrap();
    assert!(repo
        .recompute(h, h + Duration::hours(1), false)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn day_parity_ok_after_recompute_and_flags_rows_added_later() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    // A whole far-past day unique to this run; other tests' rows in that day are recomputed too,
    // so parity compares all rows of the day on both sides.
    let day_start = services::usage::trunc_day(random_past_hour());
    insert_raw(
        &f,
        day_start + Duration::hours(5),
        10,
        1,
        Some(50),
        Some("incomplete"),
        Some("external"),
    )
    .await;
    repo.recompute(day_start, day_start + Duration::days(1), true)
        .await
        .unwrap();

    let parity = repo.day_parity(day_start.date_naive()).await.unwrap();
    assert!(parity.is_ok(), "{parity:?}");

    insert_raw(
        &f,
        day_start + Duration::hours(6),
        1,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    let parity = repo.day_parity(day_start.date_naive()).await.unwrap();
    assert!(!parity.is_ok());
    assert_eq!(parity.raw.request_count - parity.aggregate.request_count, 1);
}
