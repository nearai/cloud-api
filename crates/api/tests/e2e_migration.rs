mod common;

use chrono::Utc;
use common::*;
use uuid::Uuid;

// ============================================
// Migration Tests - V0029 Backward Compatibility
// ============================================

/// Test that the usage API gracefully handles old records without V0029 migration columns
/// This simulates the scenario where the database has been migrated to V0029
/// (so the columns exist), but old usage records have NULL inference_type and inference_id,
/// requiring fallback to the deprecated request_type column
#[tokio::test]
async fn test_usage_api_with_old_v0029_records() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Get workspace and create API key
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key".to_string())
            .await;

    // Setup model with pricing
    let model_name = setup_qwen_model(&server).await;

    // Get database connection to insert old data directly
    let db_config = test_config().database;
    let db = database::Database::from_config(&db_config)
        .await
        .expect("Failed to create database connection");

    let pool = db.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    // Get model_id from database
    let model_row = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .expect("Failed to find model");
    let model_id: Uuid = model_row.get("id");

    // Parse IDs for insertion
    let org_id = org.id.parse::<Uuid>().unwrap();
    let workspace_id = workspace.id.parse::<Uuid>().unwrap();
    let api_key_id = api_key_resp.id.parse::<Uuid>().unwrap();

    // Insert 3 old usage records simulating pre-V0029 data
    // These have request_type populated but NULL inference_type and inference_id
    // (inference_type and inference_id columns not included in INSERT, so they default to NULL)
    // (response_id also omitted to avoid foreign key constraint, defaults to NULL)
    for i in 0..3 {
        client
            .execute(
                r#"
                INSERT INTO organization_usage_log (
                    id, organization_id, workspace_id, api_key_id,
                    model_id, model_name, input_tokens, output_tokens, total_tokens,
                    input_cost, output_cost, total_cost,
                    request_type, created_at,
                    ttft_ms, avg_itl_ms
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                "#,
                &[
                    &Uuid::new_v4(),
                    &org_id,
                    &workspace_id,
                    &api_key_id,
                    &model_id,
                    &model_name,
                    &(100 + i * 10),
                    &(50 + i * 5),
                    &(150 + i * 15),
                    &(1000000i64 + i as i64 * 100000),
                    &(2000000i64 + i as i64 * 200000),
                    &(3000000i64 + i as i64 * 300000),
                    &"chat_completion", // request_type (OLD column)
                    &Utc::now(),
                    &None::<i32>, // ttft_ms
                    &None::<f64>, // avg_itl_ms
                ],
            )
            .await
            .expect("Failed to insert old usage record");
    }

    println!("✓ Inserted 3 old usage records (pre-V0029 format)");

    // Update organization balance to match the inserted usage
    let total_cost = 3000000i64 + 3300000i64 + 3600000i64; // Sum of the 3 records
    client
        .execute(
            r#"
            INSERT INTO organization_balance (
                organization_id, total_spent, last_usage_at,
                total_requests, total_tokens, updated_at
            ) VALUES ($1, $2, $3, 3, $4, $3)
            ON CONFLICT (organization_id) DO UPDATE SET
                total_spent = $2,
                total_requests = 3,
                total_tokens = $4,
                last_usage_at = $3,
                updated_at = $3
            "#,
            &[
                &org_id,
                &total_cost,
                &Utc::now(),
                &(150i64 + 165i64 + 180i64), // total_tokens
            ],
        )
        .await
        .expect("Failed to update organization balance");

    println!("✓ Updated organization balance");

    // Test: Get organization usage history
    let response = server
        .get(format!("/v1/organizations/{}/usage/history?limit=10", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage history API should succeed with old records"
    );

    let usage_response: serde_json::Value =
        serde_json::from_str(&response.text()).expect("Failed to parse usage history response");

    let logs = usage_response["data"]
        .as_array()
        .expect("data should be an array");
    assert_eq!(logs.len(), 3, "Should return 3 usage records");

    // Verify all records have inference_type populated (from request_type fallback)
    for log in logs {
        assert_eq!(
            log["inference_type"].as_str().unwrap(),
            "chat_completion",
            "inference_type should be populated from request_type fallback"
        );
    }

    println!("✓ Usage history API successfully returned old records");
    println!("✓ All records have inference_type populated from request_type fallback");

    // Test: Get API key usage history
    let response2 = server
        .get(
            format!(
                "/v1/workspaces/{}/api-keys/{}/usage/history?limit=10",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        response2.status_code(),
        200,
        "API key usage history API should succeed with old records"
    );

    let api_key_usage: serde_json::Value = serde_json::from_str(&response2.text())
        .expect("Failed to parse API key usage history response");

    let api_key_logs = api_key_usage["data"]
        .as_array()
        .expect("data should be an array");
    assert_eq!(
        api_key_logs.len(),
        3,
        "Should return 3 usage records for API key"
    );

    println!("✓ API key usage history API successfully returned old records");

    // Test: Get organization balance
    let response3 = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response3.status_code(), 200, "Balance API should succeed");

    let balance: serde_json::Value =
        serde_json::from_str(&response3.text()).expect("Failed to parse balance response");

    assert_eq!(
        balance["total_requests"].as_i64().unwrap(),
        3,
        "Should show 3 total requests"
    );

    println!("✓ Organization balance API returned correct totals");
    println!("✓ Test completed: System gracefully handles old V0029 records");
}
