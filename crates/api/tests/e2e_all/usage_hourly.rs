//! usage_hourly repository: recompute, day parity, progress. Tests share a global advisory lock
//! and day parity compares all organizations, so nextest runs this module serially. `serial_`
//! tests move global progress, so they use a far-future namespace (+4000 days) and clean it up.

use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
use chrono::{DateTime, Duration, TimeZone, Utc};
use database::repositories::UsageHourlyRepositoryImpl;
use services::usage::ports::{AggregateLockBehavior, UsageHourlyRepository};

fn random_past_hour() -> DateTime<Utc> {
    let hours = (uuid::Uuid::new_v4().as_u128() % (20 * 365 * 24)) as i64;
    Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).unwrap() + Duration::hours(hours)
}

/// One raw usage row; total_tokens is input + output.
#[derive(Default)]
struct RawRow<'a> {
    input_tokens: i32,
    output_tokens: i32,
    cache_read_tokens: i32,
    total_cost: i64,
    ttft_ms: Option<i32>,
    avg_itl_ms: Option<f64>,
    stop_reason: Option<&'a str>,
    served_provider_type: Option<&'a str>,
}

async fn insert_raw_row(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
    created_at: DateTime<Utc>,
    r: RawRow<'_>,
) {
    let client = f.database.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO organization_usage_log (
                organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, cache_read_tokens,
                input_cost, output_cost, total_cost, request_type, inference_type, created_at,
                ttft_ms, avg_itl_ms, stop_reason, served_provider_type, served_via_fallback)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$6::INTEGER + $7::INTEGER,$8,$9,0,$9,'chat_completion','chat_completion',$10,
                     $11,$12,$13,$14,false)",
            &[
                &f.organization_id,
                &f.workspace_id,
                &f.api_key_id,
                &f.model_id,
                &f.model_name,
                &r.input_tokens,
                &r.output_tokens,
                &r.cache_read_tokens,
                &r.total_cost,
                &created_at,
                &r.ttft_ms,
                &r.avg_itl_ms,
                &r.stop_reason,
                &r.served_provider_type,
            ],
        )
        .await
        .unwrap();
}

async fn insert_raw(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
    created_at: DateTime<Utc>,
    total_cost: i64,
    tokens: i32,
    ttft_ms: Option<i32>,
    stop_reason: Option<&str>,
    served_provider_type: Option<&str>,
) {
    let row = RawRow {
        input_tokens: tokens,
        total_cost,
        ttft_ms,
        stop_reason,
        served_provider_type,
        ..Default::default()
    };
    insert_raw_row(f, created_at, row).await;
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
        .recompute(h, h + Duration::hours(2), AggregateLockBehavior::Wait)
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
async fn recompute_computes_every_aggregate_column_and_excludes_the_upper_bound() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let h = random_past_hour();
    let ext = Some("external");
    let rows = [
        (
            Duration::zero(),
            10,
            20,
            3,
            100,
            Some(100),
            Some(10.0),
            None,
        ),
        (
            Duration::minutes(10),
            5,
            7,
            0,
            50,
            Some(200),
            Some(20.0),
            Some("incomplete"),
        ),
        (
            Duration::minutes(20),
            1,
            2,
            1,
            10,
            Some(300),
            None,
            Some("timeout"),
        ),
        (
            Duration::minutes(30),
            4,
            0,
            0,
            5,
            Some(400),
            Some(40.0),
            Some("completed"),
        ),
        (
            Duration::milliseconds(3_599_500),
            0,
            1,
            0,
            1,
            None,
            None,
            None,
        ),
        // Exactly `to`: outside [h, h+1h), must not be counted or written.
        (
            Duration::hours(1),
            1000,
            1000,
            1000,
            1000,
            Some(9999),
            Some(999.0),
            Some("timeout"),
        ),
    ];
    for (offset, input, output, cache, cost, ttft, itl, stop) in rows {
        let row = RawRow {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache,
            total_cost: cost,
            ttft_ms: ttft,
            avg_itl_ms: itl,
            stop_reason: stop,
            served_provider_type: ext,
        };
        insert_raw_row(&f, h + offset, row).await;
    }

    repo.recompute(h, h + Duration::hours(1), AggregateLockBehavior::Wait)
        .await
        .unwrap()
        .unwrap();

    let client = f.database.pool().get().await.unwrap();
    let got = client
        .query(
            "SELECT hour, request_count, input_tokens, output_tokens, cache_read_tokens,
                    total_tokens, total_cost, error_count, incomplete_count, stop_reason_count,
                    ttft_count, ttft_sum_ms, ttft_p50_ms, ttft_p95_ms, ttft_p99_ms,
                    itl_count, itl_sum_ms, itl_p95_ms, last_usage_at
             FROM usage_hourly WHERE organization_id = $1",
            &[&f.organization_id],
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "only hour h is written");
    let r = &got[0];
    assert_eq!(r.get::<_, DateTime<Utc>>("hour"), h);
    let ints: Vec<i64> = [
        "request_count",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "total_tokens",
        "total_cost",
        "error_count",
        "incomplete_count",
        "stop_reason_count",
        "ttft_count",
        "ttft_sum_ms",
        "itl_count",
    ]
    .iter()
    .map(|c| r.get(*c))
    .collect();
    assert_eq!(ints, vec![5, 20, 30, 4, 50, 166, 1, 1, 3, 4, 1000, 3]);
    // PERCENTILE_CONT interpolates at p*(n-1): ttft [100,200,300,400], itl [10,20,40].
    for (col, want) in [
        ("ttft_p50_ms", 250.0),
        ("ttft_p95_ms", 385.0),
        ("ttft_p99_ms", 397.0),
        ("itl_sum_ms", 70.0),
        ("itl_p95_ms", 38.0),
    ] {
        let got: f64 = r.get(col);
        assert!((got - want).abs() < 1e-9, "{col}: got {got}, want {want}");
    }
    assert_eq!(
        r.get::<_, DateTime<Utc>>("last_usage_at"),
        h + Duration::milliseconds(3_599_500)
    );
}

#[tokio::test]
async fn recompute_is_idempotent_and_picks_up_late_rows() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let h = random_past_hour();
    insert_raw(&f, h, 10, 1, None, None, Some("external")).await;
    repo.recompute(h, h + Duration::hours(1), AggregateLockBehavior::Wait)
        .await
        .unwrap();
    repo.recompute(h, h + Duration::hours(1), AggregateLockBehavior::Wait)
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
    repo.recompute(h, h + Duration::hours(1), AggregateLockBehavior::Wait)
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
        .recompute(h, h + Duration::hours(1), AggregateLockBehavior::SkipIfBusy)
        .await
        .unwrap()
        .is_none());
    tx.rollback().await.unwrap();
    assert!(repo
        .recompute(h, h + Duration::hours(1), AggregateLockBehavior::SkipIfBusy)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn recompute_rejects_bounds_not_on_whole_utc_hours() {
    let f = setup_platform_provider_usage_fixture().await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let h = random_past_hour();
    for (from, to) in [
        (h + Duration::minutes(30), h + Duration::hours(2)),
        (h, h + Duration::hours(1) + Duration::seconds(1)),
        (h + Duration::milliseconds(1), h + Duration::hours(1)),
    ] {
        let err = repo
            .recompute(from, to, AggregateLockBehavior::Wait)
            .await
            .expect_err("unaligned bound must be rejected");
        assert!(
            err.to_string().contains("whole UTC hours"),
            "from={from} to={to}: {err}"
        );
    }
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
    repo.recompute(
        day_start,
        day_start + Duration::days(1),
        AggregateLockBehavior::Wait,
    )
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

/// A failed earlier run skips its end-of-test cleanup and leaves far-future rows that would
/// anchor global progress (MAX(hour)) for every later run. Only the serialized `serial_` tests
/// use that namespace, so each one clears it first.
async fn delete_leftover_far_future_rows(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
) {
    let client = f.database.pool().get().await.unwrap();
    client
        .execute(
            "DELETE FROM usage_hourly WHERE hour > NOW() + INTERVAL '1000 days'",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM organization_usage_log WHERE created_at > NOW() + INTERVAL '1000 days'",
            &[],
        )
        .await
        .unwrap();
}

async fn delete_org_future_rows(
    f: &crate::admin_provider_attribution_support::PlatformProviderUsageFixture,
) {
    let client = f.database.pool().get().await.unwrap();
    client
        .execute(
            "DELETE FROM organization_usage_log WHERE organization_id = $1",
            &[&f.organization_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM usage_hourly WHERE organization_id = $1",
            &[&f.organization_id],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn serial_progress_reports_max_hour_and_next_raw_hour() {
    let f = setup_platform_provider_usage_fixture().await;
    delete_leftover_far_future_rows(&f).await;
    let repo = UsageHourlyRepositoryImpl::new(f.database.pool().clone());
    let base = repo
        .progress()
        .await
        .unwrap()
        .max_hour
        .unwrap_or_else(Utc::now)
        .max(Utc::now());
    let a = services::usage::trunc_hour(base) + Duration::days(4000);
    let b = a + Duration::days(3);
    insert_raw(
        &f,
        a + Duration::minutes(15),
        1,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    insert_raw(
        &f,
        b + Duration::minutes(15),
        1,
        1,
        None,
        None,
        Some("external"),
    )
    .await;

    repo.recompute(a, a + Duration::hours(1), AggregateLockBehavior::Wait)
        .await
        .unwrap();
    let progress = repo.progress().await.unwrap();
    assert_eq!(progress.max_hour, Some(a));
    assert_eq!(progress.next_raw_hour, Some(b)); // jumps the empty span between a+1h and b

    delete_org_future_rows(&f).await;
}

#[tokio::test]
async fn serial_concurrent_ticks_write_each_hour_once() {
    let f = setup_platform_provider_usage_fixture().await;
    delete_leftover_far_future_rows(&f).await;
    let repo: std::sync::Arc<dyn UsageHourlyRepository> =
        std::sync::Arc::new(UsageHourlyRepositoryImpl::new(f.database.pool().clone()));
    let base = repo
        .progress()
        .await
        .unwrap()
        .max_hour
        .unwrap_or_else(Utc::now)
        .max(Utc::now());
    let h = services::usage::trunc_hour(base) + Duration::days(4000);
    // Anchor progress just before h so the planned window is deterministic regardless of
    // other tests' rows: max_hour = h-2h, next_raw_hour = h.
    insert_raw(
        &f,
        h - Duration::hours(2),
        1,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    repo.recompute(
        h - Duration::hours(2),
        h - Duration::hours(1),
        AggregateLockBehavior::Wait,
    )
    .await
    .unwrap();
    insert_raw(
        &f,
        h + Duration::minutes(1),
        3,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    // Clock so that h is inside the steady 3-hour re-read window: plan = [h-1h, h+2h).
    let now = h + Duration::hours(2) + Duration::minutes(5);

    let a = services::usage::UsageHourlyScheduler::new(repo.clone());
    let b = services::usage::UsageHourlyScheduler::new(repo.clone());
    let (ra, rb) = tokio::join!(a.run_once(now), b.run_once(now));
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert!(
        !ra.skipped || !rb.skipped,
        "at least one replica recomputes"
    );
    assert_eq!(
        org_rows(&f).await,
        vec![
            (h - Duration::hours(2), Some("external".into()), 1, 1, 0, 0),
            (h, Some("external".into()), 1, 3, 0, 0),
        ]
    );

    delete_org_future_rows(&f).await;
}
