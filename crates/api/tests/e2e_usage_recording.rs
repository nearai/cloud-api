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
            "output_tokens": 50
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
    assert!(body["created_at"].is_string(), "created_at should be present");

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
            "image_count": 3
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
    assert!(body["created_at"].is_string(), "created_at should be present");

    // Image generation response should NOT contain token fields
    assert!(body.get("input_tokens").is_none(), "input_tokens should not be in image_generation response");
    assert!(body.get("output_tokens").is_none(), "output_tokens should not be in image_generation response");
}

/// Test that the optional `id` field is accepted and does not affect the response shape.
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
            "output_tokens": 50
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
            "output_tokens": 0
        }))
        .await;

    assert_eq!(response.status_code(), 400);
}
