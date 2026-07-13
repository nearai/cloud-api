use super::usage_export_fixture::{
    assert_no_private_export_fields, redacted_export, seed_export_fixture, url_ts, ExportFixture,
};
use super::{bearer, setup_reporting_usage_server};
use crate::common::{
    create_org, get_session_id, setup_test_server_with_config_and_database, MOCK_USER_AGENT,
};
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
async fn usage_export_cursor_restores_filters_and_excludes_consumed_tied_service_rows() {
    // Given: a mixed export whose third row is inference usage tied with a service row.
    let (server, database) = setup_reporting_usage_server().await;
    let fixture = seed_export_fixture(&server, &database).await;
    let first_url = format!(
        "/v1/organizations/{}/usage/export?start_time={}&end_time={}&source=all&workspace_id={}&api_key_id={}&limit=3",
        fixture.org_id,
        url_ts(2026, 7, 1),
        url_ts(2026, 7, 4),
        fixture.workspace_id,
        fixture.api_key_id
    );
    let first = get_export(&server, &fixture, first_url).await;
    let first_rows = first["data"].as_array().expect("data should be an array");
    assert_eq!(first_rows.len(), 3);
    assert_eq!(first_rows[1]["source"], "service");
    assert_eq!(first_rows[2]["source"], "inference");
    assert_eq!(first_rows[1]["created_at"], first_rows[2]["created_at"]);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("first page should include next_cursor");

    // When: page two supplies only the cursor, omitting the original window and filters.
    let second = get_export(
        &server,
        &fixture,
        format!(
            "/v1/organizations/{}/usage/export?cursor={cursor}",
            fixture.org_id
        ),
    )
    .await;

    // Then: context is restored and the already-consumed tied service row is not duplicated.
    let second_rows = second["data"].as_array().expect("data should be an array");
    assert_eq!(second_rows.len(), 1, "{second}");
    assert_eq!(second_rows[0]["source"], "inference");
    assert_eq!(second_rows[0]["created_at"], "2026-07-01T00:00:00Z");
    assert!(second.get("next_cursor").is_none());
}

#[tokio::test]
async fn usage_export_rejects_cursor_from_another_organization() {
    let (server, database) = setup_reporting_usage_server().await;
    let first_org = seed_export_fixture(&server, &database).await;
    let second_org = seed_export_fixture(&server, &database).await;

    let first_page = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?start_time={}&end_time={}&limit=1",
                first_org.org_id,
                url_ts(2026, 7, 1),
                url_ts(2026, 7, 4)
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&first_org.token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(first_page.status_code(), 200, "{}", first_page.text());
    let cursor = first_page
        .json::<Value>()
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("first organization should return a continuation cursor")
        .to_string();

    let replay = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?cursor={cursor}",
                second_org.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&second_org.token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(replay.status_code(), 400, "{}", replay.text());
    assert_eq!(
        replay.json::<Value>()["error"]["type"],
        "invalid_reporting_usage_query"
    );
}

#[tokio::test]
async fn usage_export_database_timeout_returns_504_and_stops_query() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.usage_reporting.request_timeout_seconds = 1;
    })
    .await;
    let org = create_org(&server).await;
    let token = super::create_reporting_token(&server, &org.id).await;
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

    let response = server
        .get(format!("/v1/organizations/{}/usage/export?source=inference", org.id).as_str())
        .add_header("Authorization", bearer(&token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 504, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "reporting_request_timeout"
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
    assert_eq!(active_query_count, 0, "timed-out export query must stop");
    transaction.rollback().await.expect("release test lock");
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

    let first_page = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?start_time={}&end_time={}&source=all&limit=1",
                fixture.org_id,
                url_ts(2026, 7, 1),
                url_ts(2026, 7, 4)
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&fixture.token))
        .await
        .json::<Value>();
    let bound_cursor = first_page["next_cursor"]
        .as_str()
        .expect("first page should include next_cursor");
    let conflicting_filter = server
        .get(
            format!(
                "/v1/organizations/{}/usage/export?cursor={bound_cursor}&source=service",
                fixture.org_id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&fixture.token))
        .await;
    assert_eq!(
        conflicting_filter.status_code(),
        400,
        "{}",
        conflicting_filter.text()
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
