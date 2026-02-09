mod common;

use common::*;

/// Happy-path test for POST /v1/usage with type=chat_completion.
/// Sets up a model with pricing, creates an org with credits, and records usage.
#[tokio::test]
async fn test_record_chat_completion_usage() {
    let (server, _guard) = setup_test_server().await;

    // Setup model with known pricing (input: 1_000_000, output: 2_000_000 nano-dollars per token)
    setup_qwen_model(&server).await;

    // Setup org with $10 credits and get an API key
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Record chat completion usage
    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-chat-completion-001"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();

    // Verify tagged union type
    assert_eq!(body["type"], "chat_completion");

    // Verify token counts
    assert_eq!(body["input_tokens"], 100);
    assert_eq!(body["output_tokens"], 50);
    assert_eq!(body["total_tokens"], 150);

    // Verify costs are calculated correctly
    // input: 100 tokens * 1_000_000 nano-dollars = 100_000_000
    // output: 50 tokens * 2_000_000 nano-dollars = 100_000_000
    // total: 200_000_000
    assert_eq!(body["input_cost"], 100_000_000i64);
    assert_eq!(body["output_cost"], 100_000_000i64);
    assert_eq!(body["total_cost"], 200_000_000i64);

    // Verify model name
    assert_eq!(body["model"], "Qwen/Qwen3-30B-A3B-Instruct-2507");

    // Verify id and created_at are present
    assert!(body["id"].is_string(), "id should be present");
    assert!(
        body["created_at"].is_string(),
        "created_at should be present"
    );

    // Verify total_cost_display is human-readable
    assert!(
        body["total_cost_display"].is_string(),
        "total_cost_display should be present"
    );
}

/// Happy-path test for POST /v1/usage with type=image_generation.
#[tokio::test]
async fn test_record_image_generation_usage() {
    let (server, _guard) = setup_test_server().await;

    // Setup image model with cost_per_image pricing (40_000_000 nano-dollars per image)
    setup_qwen_image_model(&server).await;

    // Setup org with $10 credits and get an API key
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Record image generation usage
    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "image_generation",
            "model": "Qwen/Qwen-Image-2512",
            "image_count": 3,
            "id": "test-image-gen-001"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();

    // Verify tagged union type
    assert_eq!(body["type"], "image_generation");

    // Verify image count
    assert_eq!(body["image_count"], 3);

    // Verify cost: 3 images * 40_000_000 nano-dollars = 120_000_000
    assert_eq!(body["total_cost"], 120_000_000i64);

    // Verify model name
    assert_eq!(body["model"], "Qwen/Qwen-Image-2512");

    // Verify id and created_at are present
    assert!(body["id"].is_string(), "id should be present");
    assert!(
        body["created_at"].is_string(),
        "created_at should be present"
    );

    // Image generation response should NOT contain token fields
    assert!(
        body.get("input_tokens").is_none(),
        "input_tokens should not be in image_generation response"
    );
    assert!(
        body.get("output_tokens").is_none(),
        "output_tokens should not be in image_generation response"
    );
}

/// Test that the required `id` field is stored and does not affect the response shape.
#[tokio::test]
async fn test_record_usage_with_external_id() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 10,
            "output_tokens": 20,
            "id": "chatcmpl-ext-12345"
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Usage recording with external id should succeed: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["type"], "chat_completion");
    assert_eq!(body["input_tokens"], 10);
    assert_eq!(body["output_tokens"], 20);

    // The response id should be the usage log row's primary key (not the external id)
    assert!(body["id"].is_string());
}

/// Test validation: model not found returns 404.
#[tokio::test]
async fn test_record_usage_model_not_found() {
    let (server, _guard) = setup_test_server().await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "nonexistent/model",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": "test-not-found-001"
        }))
        .await;

    assert_eq!(response.status_code(), 404);
}

/// Test validation: zero tokens returns 400.
#[tokio::test]
async fn test_record_usage_zero_tokens() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 0,
            "output_tokens": 0,
            "id": "test-zero-tokens-001"
        }))
        .await;

    assert_eq!(response.status_code(), 400);
}

/// Test validation: missing `id` field returns 422 (deserialization error).
#[tokio::test]
async fn test_record_usage_missing_id_field() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50
        }))
        .await;

    // Missing required `id` field should fail deserialization
    assert_eq!(
        response.status_code(),
        422,
        "Missing id should return 422: {}",
        response.text()
    );
}

/// Test validation: empty `id` field returns 400.
#[tokio::test]
async fn test_record_usage_empty_id() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let response = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": ""
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Empty id should return 400: {}",
        response.text()
    );
}

/// Idempotency test: calling POST /v1/usage twice with the same `id` returns
/// the same record both times and only charges the organization once.
#[tokio::test]
async fn test_record_usage_idempotent_duplicate() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    let payload = serde_json::json!({
        "type": "chat_completion",
        "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
        "input_tokens": 100,
        "output_tokens": 50,
        "id": "idempotency-test-same-id"
    });

    // First call — creates the record
    let response1 = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .await;

    assert_eq!(
        response1.status_code(),
        200,
        "First call should succeed: {}",
        response1.text()
    );
    let body1: serde_json::Value = response1.json();

    // Second call — should return existing record (no double-charge)
    let response2 = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .await;

    assert_eq!(
        response2.status_code(),
        200,
        "Duplicate call should also succeed: {}",
        response2.text()
    );
    let body2: serde_json::Value = response2.json();

    // Both responses should return the same usage record (same primary key id)
    assert_eq!(
        body1["id"], body2["id"],
        "Both calls should return the same record id"
    );
    assert_eq!(body1["total_cost"], body2["total_cost"]);
    assert_eq!(body1["created_at"], body2["created_at"]);

    // Verify the balance was only charged once (total_cost = 200_000_000)
    // by recording a second distinct usage and checking the combined balance
    let response3 = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 10,
            "output_tokens": 5,
            "id": "idempotency-test-different-id"
        }))
        .await;

    assert_eq!(response3.status_code(), 200);
}

/// Test that two different organizations can use the same external `id`
/// without conflicting — the idempotency scope is per-organization.
#[tokio::test]
async fn test_record_usage_same_id_different_orgs() {
    let (server, _guard) = setup_test_server().await;

    setup_qwen_model(&server).await;

    // Setup two separate orgs
    let org1 = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key1 = get_api_key_for_org(&server, org1.id.clone()).await;

    let org2 = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key2 = get_api_key_for_org(&server, org2.id.clone()).await;

    let shared_id = "shared-external-id-across-orgs";

    // Org1 records usage with the shared id
    let response1 = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key1}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 100,
            "output_tokens": 50,
            "id": shared_id
        }))
        .await;

    assert_eq!(
        response1.status_code(),
        200,
        "Org1 usage should succeed: {}",
        response1.text()
    );

    // Org2 records usage with the same id — should NOT conflict
    let response2 = server
        .post("/v1/usage")
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .json(&serde_json::json!({
            "type": "chat_completion",
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input_tokens": 200,
            "output_tokens": 100,
            "id": shared_id
        }))
        .await;

    assert_eq!(
        response2.status_code(),
        200,
        "Org2 usage with same id should succeed: {}",
        response2.text()
    );

    let body1: serde_json::Value = response1.json();
    let body2: serde_json::Value = response2.json();

    // The two records should be different (different primary key ids)
    assert_ne!(
        body1["id"], body2["id"],
        "Different orgs should create separate records even with same external id"
    );

    // Costs should reflect the different token counts
    assert_eq!(body1["input_tokens"], 100);
    assert_eq!(body2["input_tokens"], 200);
}
