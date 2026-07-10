use super::usage_export_fixture::{
    assert_no_private_export_fields, redacted_export, seed_export_fixture, url_ts, ExportFixture,
};
use super::{bearer, setup_reporting_usage_server};
use crate::common::{create_org, get_session_id, MOCK_USER_AGENT};
use serde_json::Value;

#[tokio::test]
async fn usage_export() {
    // Given: one organization with inference and service usage sharing a timestamp.
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let base = format!(
        "/v1/organizations/{}/usage/export?start_time={}&end_time={}&workspace_id={}&api_key_id={}",
        fixture.org_id,
        url_ts(2026, 7, 1),
        url_ts(2026, 7, 4),
        fixture.workspace_id,
        fixture.api_key_id
    );

    // When: the mixed export is fetched with a small page size.
    let first = server
        .get(format!("{base}&source=all&limit=2").as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: rows are merged by created_at, source, id and return a cursor.
    assert_eq!(first.status_code(), 200, "{}", first.text());
    let first_body = first.json::<Value>();
    assert_no_private_export_fields(&first_body);
    let first_rows = first_body["data"]
        .as_array()
        .expect("data should be an array");
    assert_eq!(first_rows.len(), 2);
    assert_eq!(first_rows[0]["source"], "service");
    assert_eq!(first_rows[1]["source"], "service");
    let cursor = first_body["next_cursor"]
        .as_str()
        .expect("first page should include next_cursor");
    println!(
        "manual GET /usage/export source=all limit=2 200 {}",
        redacted_export(&first_body)
    );

    // When: the next cursor is used.
    let second = server
        .get(format!("{base}&source=all&limit=2&cursor={cursor}").as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: continuation returns the tied inference row without duplicate/skip.
    assert_eq!(second.status_code(), 200, "{}", second.text());
    let second_body = second.json::<Value>();
    assert_no_private_export_fields(&second_body);
    let second_rows = second_body["data"]
        .as_array()
        .expect("data should be an array");
    assert_eq!(second_rows.len(), 2);
    assert_eq!(second_rows[0]["source"], "inference");
    assert_eq!(second_rows[1]["source"], "inference");
    assert!(second_body.get("next_cursor").is_none());

    // When/Then: source-specific filters return only applicable rows.
    let inference = get_export(
        &server,
        &fixture,
        format!("{base}&source=inference&model={}", fixture.model),
    )
    .await;
    assert_eq!(inference["data"].as_array().expect("array").len(), 2);
    assert!(inference["data"].as_array().expect("array")[0]["service"].is_null());

    let service = get_export(
        &server,
        &fixture,
        format!(
            "{base}&source=service&service_name={}",
            fixture.service_name
        ),
    )
    .await;
    assert_eq!(service["data"].as_array().expect("array").len(), 2);
    assert!(service["data"].as_array().expect("array")[0]["inference"].is_null());

    let empty = get_export(
        &server,
        &fixture,
        format!("{base}&source=inference&model=none"),
    )
    .await;
    assert!(empty["data"].as_array().expect("array").is_empty());
    assert!(empty.get("next_cursor").is_none());
}

#[tokio::test]
async fn usage_export_omits_cache_read_cost_when_not_separately_persisted() {
    // Given: persisted inference usage includes cached tokens but no separate cache-read cost column.
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let url = format!(
        "/v1/organizations/{}/usage/export?start_time={}&end_time={}&source=inference&workspace_id={}&api_key_id={}&model={}",
        fixture.org_id,
        url_ts(2026, 7, 1),
        url_ts(2026, 7, 4),
        fixture.workspace_id,
        fixture.api_key_id,
        fixture.model
    );

    // When: inference usage is exported.
    let body = get_export(&server, &fixture, url).await;

    // Then: the cache token count is reported, but no cache-read cost is invented.
    let rows = body["data"].as_array().expect("data should be an array");
    assert_eq!(rows.len(), 2);
    let inference = &rows[0]["inference"];
    assert_eq!(inference["cache_read_tokens"].as_i64(), Some(1));
    assert_eq!(inference["input_cost_nano_usd"].as_i64(), Some(400));
    assert_eq!(inference["output_cost_nano_usd"].as_i64(), Some(300));
    assert_eq!(inference["total_cost_nano_usd"].as_i64(), Some(700));
    assert!(
        inference.get("cache_read_cost_nano_usd").is_none(),
        "cache-read cost split should be omitted when unavailable: {inference}"
    );
    println!(
        "manual GET /usage/export source=inference cache_read_cost_nano_usd omitted contract 200 {}",
        redacted_export(&body)
    );
}

#[tokio::test]
async fn usage_export_rejects_bad_input_and_bad_auth_scope() {
    // Given: two organizations and one active reporting token.
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let other_org = create_org(&server).await;

    // When/Then: malformed input is rejected by the shared validators.
    let invalid_range = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?start_time={}&end_time={}",
                fixture.org_id,
                url_ts(2026, 7, 4),
                url_ts(2026, 7, 1)
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(invalid_range.status_code(), 400, "{}", invalid_range.text());

    let invalid_cursor = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?cursor=bad",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(
        invalid_cursor.status_code(),
        400,
        "{}",
        invalid_cursor.text()
    );

    // When: a token from one org calls another org export.
    let mismatch = server
        .get(format!("/v1/organizations/{}/usage/export", other_org.id).as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .await;

    // Then: org mismatch uses the established forbidden envelope.
    assert_eq!(mismatch.status_code(), 403, "{}", mismatch.text());
    println!(
        "manual GET /usage/export org mismatch 403 {}",
        mismatch.text()
    );

    // When/Then: revoked reporting tokens cannot export usage.
    revoke_reporting_token(&server, &fixture).await;
    let revoked = server
        .get(format!("/v1/organizations/{}/usage/export", fixture.org_id).as_str())
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(revoked.status_code(), 401, "{}", revoked.text());
}

async fn get_export(server: &axum_test::TestServer, fixture: &ExportFixture, url: String) -> Value {
    let response = server
        .get(&url)
        .add_header("Authorization", bearer(&fixture.token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<Value>();
    assert_no_private_export_fields(&body);
    body
}

async fn revoke_reporting_token(server: &axum_test::TestServer, fixture: &ExportFixture) {
    let list = server
        .get(format!("/v1/organizations/{}/reporting-tokens", fixture.org_id).as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<Value>();
    let token_id = list["reporting_tokens"]
        .as_array()
        .expect("tokens array")
        .iter()
        .find(|token| token["token_prefix"] == fixture.token[..12])
        .and_then(|token| token["id"].as_str())
        .expect("token id");
    let response = server
        .delete(
            format!(
                "/v1/organizations/{}/reporting-tokens/{token_id}",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 204, "{}", response.text());
}
