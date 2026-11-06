// Import common test utilities

use crate::common::*;
use serde_json::json;

/// Test that system prompt is applied to a stateless Responses request.
#[tokio::test]
async fn test_system_prompt_integration_with_stateless_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    setup_glm_model(&server).await;
    setup_qwen_model(&server).await;

    // Set system prompt
    seeding::seed_organization_settings(
        &org.id,
        json!({ "system_prompt": "You are a test assistant." }),
    )
    .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "Hello",
            "store": false,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_json: serde_json::Value = response.json();
    assert_eq!(response_json["status"], "completed");
}
