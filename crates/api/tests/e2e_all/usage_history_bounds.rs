//! Bounds on the organization usage dashboard queries: the unfiltered history
//! total comes from the balance counter instead of a scan of the organization's
//! rows, and every history and by-model query stops at the statement timeout.

use crate::common::*;
use chrono::{Duration, SecondsFormat, Utc};
use serde_json::Value;
use uuid::Uuid;

const INTERNAL_USAGE_TOKEN: &str = "usage-history-bounds-secret";

#[tokio::test]
async fn usage_by_model_combines_hourly_counts_with_recent_requests() {
    use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
    use crate::usage_hourly::{insert_raw, recompute_usage_hours};

    let fixture = setup_platform_provider_usage_fixture().await;
    let hour = services::usage::trunc_hour(Utc::now() - Duration::days(1));
    // Two requests collapse into one aggregate row; count requests, not rows.
    for _ in 0..2 {
        insert_raw(&fixture, hour, 100, 10, None, None, Some("external")).await;
    }
    recompute_usage_hours(hour, hour + Duration::hours(1)).await;
    insert_raw(&fixture, Utc::now(), 7, 3, None, None, Some("external")).await;

    let response = fixture
        .server
        .get(&format!(
            "/v1/organizations/{}/usage/by-model?period=month",
            fixture.organization_id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<Value>();
    let rows = body["data"].as_array().expect("by-model entries");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["model"], fixture.model_name);
    assert_eq!(rows[0]["request_count"], 3);
    assert_eq!(rows[0]["input_tokens"], 23);
    assert_eq!(rows[0]["total_tokens"], 23);
    assert_eq!(rows[0]["total_cost"], 207);
}

#[tokio::test]
async fn usage_by_model_custom_range_includes_start_and_excludes_end() {
    use crate::admin_provider_attribution_support::setup_platform_provider_usage_fixture;
    use crate::usage_hourly::{insert_raw, random_past_hour, recompute_usage_hours};

    let fixture = setup_platform_provider_usage_fixture().await;
    // Both bounds fall mid-hour: the range reads raw rows at each edge and the hourly
    // aggregate for the whole hours between them.
    let hour = random_past_hour();
    let start = hour + Duration::minutes(30);
    let end = hour + Duration::hours(3) + Duration::minutes(30);
    // Each cost is its own decimal digit, so the summed cost names the rows counted.
    for (created_at, cost) in [
        (start - Duration::milliseconds(1), 1),
        (start, 10),
        (hour + Duration::hours(1), 100),
        (hour + Duration::hours(2) + Duration::minutes(59), 1_000),
        (end - Duration::milliseconds(1), 10_000),
        (end, 100_000),
    ] {
        insert_raw(&fixture, created_at, cost, 1, None, None, Some("external")).await;
    }
    recompute_usage_hours(hour, hour + Duration::hours(4)).await;

    let range = format!(
        "start={}&end={}",
        start.to_rfc3339_opts(SecondsFormat::Secs, true),
        end.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    // `start`/`end` select the custom range with or without a `period`.
    for query in [
        range.clone(),
        format!("period=custom&{range}"),
        format!("period=day&{range}"),
    ] {
        let body = get_by_model(&fixture.server, fixture.organization_id, &query).await;
        assert_eq!(body["period"], "custom", "{query}");
        assert_eq!(body["start_date"], start.to_rfc3339(), "{query}");
        assert_eq!(body["end_date"], end.to_rfc3339(), "{query}");
        let rows = body["data"].as_array().expect("by-model entries");
        assert_eq!(rows.len(), 1, "{query}");
        assert_eq!(rows[0]["request_count"], 4, "{query}");
        assert_eq!(rows[0]["total_cost"], 11_110, "{query}");
    }

    // A rolling window is open-ended and far from these rows.
    let body = get_by_model(&fixture.server, fixture.organization_id, "period=month").await;
    assert_eq!(body["period"], "month");
    assert!(body.get("end_date").is_none());
    assert!(body["data"]
        .as_array()
        .expect("by-model entries")
        .is_empty());
}

#[tokio::test]
async fn usage_by_model_validates_custom_range() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // 366 days is the longest range allowed.
    let longest = "start=2025-01-01T00:00:00Z&end=2026-01-02T00:00:00Z";
    assert_eq!(
        get_by_model(&server, &org.id, longest).await["period"],
        "custom"
    );

    for (query, error_type) in [
        (
            "start=2025-01-01T00:00:00Z&end=2026-01-02T00:00:01Z",
            "date_range_too_large",
        ),
        (
            "start=2026-01-02T00:00:00Z&end=2026-01-01T00:00:00Z",
            "invalid_date_range",
        ),
        // `end` defaults to now, so a future `start` is an empty range.
        ("start=2999-01-01T00:00:00Z", "invalid_date_range"),
        ("end=2026-01-01", "invalid_date"),
    ] {
        let response = server
            .get(format!("/v1/organizations/{}/usage/by-model?{query}", org.id).as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await;
        assert_eq!(response.status_code(), 400, "{query}: {}", response.text());
        assert_eq!(
            response.json::<Value>()["error"]["type"],
            error_type,
            "{query}"
        );
    }
}

#[tokio::test]
async fn usage_history_total_is_the_recorded_request_count() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.internal_usage_token = Some(INTERNAL_USAGE_TOKEN.to_string());
    })
    .await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let workspace_id = list_workspaces(&server, org.id.clone())
        .await
        .first()
        .expect("org should have a default workspace")
        .id
        .clone();
    let api_key = create_api_key_in_workspace(
        &server,
        workspace_id.clone(),
        "usage-history-bounds".to_string(),
    )
    .await;

    for request in 0..3 {
        let response = server
            .post("/v1/internal/usage")
            .add_header("Authorization", format!("Bearer {INTERNAL_USAGE_TOKEN}"))
            .json(&serde_json::json!({
                "organization_id": org.id,
                "workspace_id": workspace_id,
                "api_key_id": api_key.id,
                "type": "chat_completion",
                "model": model,
                "input_tokens": 10,
                "output_tokens": 5,
                "id": format!("usage-history-bounds-{}-{request}", Uuid::new_v4()),
            }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }

    let history = get_history(&server, &org.id, 1).await;
    assert_eq!(history.data.len(), 1);
    assert_eq!(history.total, 3);
    let balance = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await
        .json::<api::routes::usage::OrganizationBalanceResponse>();
    assert_eq!(
        history.total as i64, balance.total_requests,
        "history total should match the balance's request count"
    );

    // The total must come from the counter, not a scan of the organization's rows:
    // that scan grows with the whole history. A row removed without decrementing
    // the counter (as the V0045 duplicate cleanup did) stays in the total.
    let organization_id = Uuid::parse_str(&org.id).expect("organization id");
    let deleted = database
        .pool()
        .get()
        .await
        .expect("database connection")
        .execute(
            r#"
            DELETE FROM organization_usage_log
            WHERE id = (
                SELECT id FROM organization_usage_log
                WHERE organization_id = $1
                ORDER BY created_at
                LIMIT 1
            )
            "#,
            &[&organization_id],
        )
        .await
        .expect("delete one usage row");
    assert_eq!(deleted, 1);
    let history = get_history(&server, &org.id, 100).await;
    assert_eq!(history.data.len(), 2);
    assert_eq!(history.total, 3);
}

#[tokio::test]
async fn usage_dashboard_database_timeout_returns_504_and_stops_query() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.usage_reporting.request_timeout_seconds = 1;
    })
    .await;
    let org = create_org(&server).await;
    let mut blocker = database
        .pool()
        .get()
        .await
        .expect("blocking database connection");
    let transaction = blocker.transaction().await.expect("blocking transaction");
    transaction
        .batch_execute("LOCK TABLE organization_usage_log IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("exclusive test lock");
    let application_name: String = transaction
        .query_one("SHOW application_name", &[])
        .await
        .expect("test pool application name")
        .get(0);

    for path in [
        "usage/history?limit=20&offset=0",
        "usage/history?limit=20&offset=0&start_date=2026-01-01",
        "usage/by-model?period=month",
    ] {
        let response = server
            .get(format!("/v1/organizations/{}/{path}", org.id).as_str())
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .await;

        assert_eq!(response.status_code(), 504, "{path}: {}", response.text());
        assert_eq!(
            response.json::<Value>()["error"]["type"],
            "usage_query_timeout",
            "{path}"
        );
        let active_query_count: i64 = transaction
            .query_one(
                r#"
                SELECT COUNT(*)::BIGINT
                FROM pg_stat_activity
                WHERE datname = current_database()
                  AND pid <> pg_backend_pid()
                  AND application_name = $1
                  AND state = 'active'
                  AND query LIKE '%FROM organization_usage_log%'
                "#,
                &[&application_name],
            )
            .await
            .expect("active query check")
            .get(0);
        assert_eq!(active_query_count, 0, "{path}: timed-out query must stop");
    }
    transaction.rollback().await.expect("release test lock");
}

async fn get_by_model(
    server: &axum_test::TestServer,
    organization_id: impl std::fmt::Display,
    query: &str,
) -> Value {
    let response = server
        .get(format!("/v1/organizations/{organization_id}/usage/by-model?{query}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200, "{query}: {}", response.text());
    response.json()
}

async fn get_history(
    server: &axum_test::TestServer,
    org_id: &str,
    limit: i64,
) -> api::routes::usage::UsageHistoryResponse {
    let response = server
        .get(format!("/v1/organizations/{org_id}/usage/history?limit={limit}&offset=0").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}
