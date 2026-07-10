use super::{
    bearer, create_reporting_token, setup_reporting_usage_server,
    usage_export_fixture::{
        assert_no_private_export_fields, seed_export_fixture, url_ts, ExportFixture,
    },
};
use crate::common::create_org;
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
    assert_totals(&json, 2, 2, 12, 6, 2, 18, 1_200, 300);
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
async fn usage_summary_applies_source_specific_zero_and_empty_contracts() {
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;

    let inference = get_default_summary(&server, &fixture, "source=inference").await;
    assert_totals(&inference, 2, 0, 12, 6, 2, 18, 1_200, 0);
    assert!(inference["by_service"]
        .as_array()
        .expect("by_service")
        .is_empty());
    assert_eq!(inference["by_model"][0]["total_cost_nano_usd"], 1_200);

    let service = get_default_summary(&server, &fixture, "source=service").await;
    assert_totals(&service, 0, 2, 0, 0, 0, 0, 0, 300);
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

    assert_totals(&json, 1, 1, 7, 3, 1, 10, 700, 200);
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
    assert_totals(&json, 2, 2, 12, 6, 2, 18, 1_200, 300);
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

fn assert_totals(
    json: &Value,
    request_count: i64,
    service_usage_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_tokens: i64,
    inference_cost: i64,
    service_cost: i64,
) {
    let totals = &json["totals"];
    assert_eq!(totals["request_count"], request_count);
    assert_eq!(totals["service_usage_count"], service_usage_count);
    assert_eq!(totals["input_tokens"], input_tokens);
    assert_eq!(totals["output_tokens"], output_tokens);
    assert_eq!(totals["cache_read_tokens"], cache_read_tokens);
    assert_eq!(totals["total_tokens"], total_tokens);
    assert_eq!(totals["inference_cost_nano_usd"], inference_cost);
    assert_eq!(totals["service_cost_nano_usd"], service_cost);
    assert_eq!(totals["total_cost_nano_usd"], inference_cost + service_cost);
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
