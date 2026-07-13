use super::{
    bearer, create_reporting_token, setup_reporting_usage_server,
    usage_export_fixture::{
        assert_no_private_export_fields, seed_export_fixture, url_ts, ExportFixture,
    },
};
use crate::common::{create_org, setup_test_server_with_config_and_database, MOCK_USER_AGENT};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

#[tokio::test]
async fn usage_summary_returns_all_source_breakdowns_and_reconciles_cost() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;

    let response = server
        .get(default_summary_url(&fixture, "source=all").as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .await;

    assert_eq!(response.status_code(), 200, "{}", response.text());
    let json = response.json::<Value>();
    assert_no_private_export_fields(&json);
    assert_totals(
        &json,
        ExpectedTotals {
            request_count: 2,
            service_usage_count: 2,
            input_tokens: 12,
            output_tokens: 6,
            cache_read_tokens: 2,
            total_tokens: 18,
            inference_cost: 1_200,
            service_cost: 300,
        },
    );
    assert_eq!(json["totals"]["total_cost_nano_usd"], 1_500);
    assert_eq!(json["by_workspace"][0]["request_count"], 2);
    assert_eq!(json["by_workspace"][0]["service_usage_count"], 2);
    assert_eq!(json["by_workspace"][0]["total_cost_nano_usd"], 1_500);
    assert_eq!(json["by_api_key"][0]["request_count"], 2);
    assert_eq!(json["by_api_key"][0]["service_usage_count"], 2);
    assert_eq!(json["by_model"][0]["model"], fixture.model);
    assert_eq!(json["by_model"][0]["total_cost_nano_usd"], 1_200);
    assert_eq!(json["by_service"][0]["service_name"], fixture.service_name);
    assert_eq!(json["by_service"][0]["usage_count"], 2);
    assert_eq!(json["by_service"][0]["quantity"], 3);
    assert_eq!(json["by_service"][0]["total_cost_nano_usd"], 300);
    assert_day(&json, "2026-07-01", 1, 0, 500, 0);
    assert_day(&json, "2026-07-02", 1, 1, 700, 200);
    assert_day(&json, "2026-07-03", 0, 1, 0, 100);
    println!(
        "manual GET /usage/summary source=all 200 {}",
        redacted_summary(&json)
    );
}

#[tokio::test]
async fn usage_summary_database_timeout_returns_504() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.usage_reporting.request_timeout_seconds = 1;
    })
    .await;
    let org = create_org(&server).await;
    let token = create_reporting_token(&server, &org.id).await;
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

    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/summary?source=inference",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 504, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "reporting_request_timeout"
    );
    transaction.rollback().await.expect("release test lock");
}

#[tokio::test]
async fn usage_summary_applies_source_specific_zero_and_empty_contracts() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;

    let inference = get_default_summary(&server, &fixture, "source=inference").await;
    assert_totals(
        &inference,
        ExpectedTotals {
            request_count: 2,
            service_usage_count: 0,
            input_tokens: 12,
            output_tokens: 6,
            cache_read_tokens: 2,
            total_tokens: 18,
            inference_cost: 1_200,
            service_cost: 0,
        },
    );
    assert!(inference["by_service"]
        .as_array()
        .expect("by_service")
        .is_empty());
    assert_eq!(inference["by_model"][0]["total_cost_nano_usd"], 1_200);

    let service = get_default_summary(&server, &fixture, "source=service").await;
    assert_totals(
        &service,
        ExpectedTotals {
            request_count: 0,
            service_usage_count: 2,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 0,
            inference_cost: 0,
            service_cost: 300,
        },
    );
    assert!(service["by_model"].as_array().expect("by_model").is_empty());
    assert_eq!(service["by_service"][0]["total_cost_nano_usd"], 300);
}

#[tokio::test]
async fn usage_summary_honors_date_boundaries_and_filters() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let query = format!(
        "source=all&start_time={}&end_time={}&workspace_id={}&api_key_id={}",
        url_ts(2026, 7, 2),
        url_ts(2026, 7, 2),
        fixture.workspace_id,
        fixture.api_key_id
    );

    let json = get_summary(&server, &fixture, &query).await;

    assert_totals(
        &json,
        ExpectedTotals {
            request_count: 1,
            service_usage_count: 1,
            input_tokens: 7,
            output_tokens: 3,
            cache_read_tokens: 1,
            total_tokens: 10,
            inference_cost: 700,
            service_cost: 200,
        },
    );
    assert_eq!(json["by_day"].as_array().expect("by_day").len(), 1);
    assert_day(&json, "2026-07-02", 1, 1, 700, 200);
}

#[tokio::test]
async fn usage_summary_normalizes_open_ended_range_defaults() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let before_request = Utc::now();

    let response = server
        .get(summary_url(&fixture, "source=all").as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    let after_request = Utc::now();

    assert_eq!(response.status_code(), 200, "{}", response.text());
    let json = response.json::<Value>();
    let start_time = json_timestamp(&json, "start_time");
    let end_time = json_timestamp(&json, "end_time");
    assert!(end_time >= before_request);
    assert!(end_time <= after_request);
    assert_eq!(end_time - start_time, Duration::days(366));
    assert_totals(
        &json,
        ExpectedTotals {
            request_count: 2,
            service_usage_count: 2,
            input_tokens: 12,
            output_tokens: 6,
            cache_read_tokens: 2,
            total_tokens: 18,
            inference_cost: 1_200,
            service_cost: 300,
        },
    );
    println!(
        "manual GET /usage/summary source=all open-ended 200 {}",
        redacted_summary(&json)
    );
}

#[tokio::test]
async fn usage_summary_rejects_invalid_range_org_mismatch_and_revoked_token() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;

    let invalid = server
        .get(
            summary_url(
                &fixture,
                format!(
                    "start_time={}&end_time={}",
                    url_ts(2026, 7, 4),
                    url_ts(2026, 7, 1)
                )
                .as_str(),
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(invalid.status_code(), 400, "{}", invalid.text());
    println!(
        "manual GET /usage/summary invalid range 400 {}",
        invalid.text()
    );

    let other_org = create_org(&server).await;
    let other_token = create_reporting_token(&server, &other_org.id).await;
    let mismatch = server
        .get(default_summary_url(&fixture, "source=all").as_str())
        .add_header("Authorization", bearer(&other_token))
        .await;
    assert_eq!(mismatch.status_code(), 403, "{}", mismatch.text());

    let revoked = server
        .get(default_summary_url(&fixture, "source=all").as_str())
        .add_header("Authorization", bearer("rpt-revoked-or-malformed"))
        .await;
    assert_eq!(revoked.status_code(), 401, "{}", revoked.text());
}

async fn get_default_summary(
    server: &axum_test::TestServer,
    fixture: &ExportFixture,
    query: &str,
) -> Value {
    get_summary(server, fixture, default_summary_query(query).as_str()).await
}

async fn get_summary(
    server: &axum_test::TestServer,
    fixture: &ExportFixture,
    query: &str,
) -> Value {
    let response = server
        .get(summary_url(fixture, query).as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json::<Value>()
}

fn summary_url(fixture: &ExportFixture, query: &str) -> String {
    format!(
        "/v1/organizations/{}/usage/summary?{}",
        fixture.org_id, query
    )
}

fn default_summary_url(fixture: &ExportFixture, query: &str) -> String {
    summary_url(fixture, default_summary_query(query).as_str())
}

fn default_summary_query(query: &str) -> String {
    format!(
        "start_time={}&end_time={}&{}",
        url_ts(2026, 7, 1),
        url_ts(2026, 7, 3),
        query
    )
}

struct ExpectedTotals {
    request_count: i64,
    service_usage_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_tokens: i64,
    inference_cost: i64,
    service_cost: i64,
}

fn assert_totals(json: &Value, expected: ExpectedTotals) {
    let totals = &json["totals"];
    assert_eq!(totals["request_count"], expected.request_count);
    assert_eq!(totals["service_usage_count"], expected.service_usage_count);
    assert_eq!(totals["input_tokens"], expected.input_tokens);
    assert_eq!(totals["output_tokens"], expected.output_tokens);
    assert_eq!(totals["cache_read_tokens"], expected.cache_read_tokens);
    assert_eq!(totals["total_tokens"], expected.total_tokens);
    assert_eq!(totals["inference_cost_nano_usd"], expected.inference_cost);
    assert_eq!(totals["service_cost_nano_usd"], expected.service_cost);
    assert_eq!(
        totals["total_cost_nano_usd"],
        expected.inference_cost + expected.service_cost
    );
}

fn assert_day(
    json: &Value,
    day: &str,
    request_count: i64,
    service_usage_count: i64,
    inference_cost: i64,
    service_cost: i64,
) {
    let days = json["by_day"].as_array().expect("by_day array");
    let day_row = days
        .iter()
        .find(|row| row["day"] == day)
        .unwrap_or_else(|| panic!("missing day {day}: {days:?}"));
    assert_eq!(day_row["request_count"], request_count);
    assert_eq!(day_row["service_usage_count"], service_usage_count);
    assert_eq!(day_row["inference_cost_nano_usd"], inference_cost);
    assert_eq!(day_row["service_cost_nano_usd"], service_cost);
    assert_eq!(
        day_row["total_cost_nano_usd"],
        inference_cost + service_cost
    );
}

fn json_timestamp(json: &Value, field: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(json[field].as_str().expect(field))
        .expect(field)
        .with_timezone(&Utc)
}

fn redacted_summary(value: &Value) -> Value {
    value.clone()
}
