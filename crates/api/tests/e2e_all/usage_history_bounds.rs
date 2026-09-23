//! Bounds on the organization usage dashboard queries: the unfiltered history
//! total comes from the balance counter instead of a scan of the organization's
//! rows, and every history and by-model query stops at the statement timeout.

use crate::common::*;
use serde_json::Value;
use uuid::Uuid;

const INTERNAL_USAGE_TOKEN: &str = "usage-history-bounds-secret";

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
