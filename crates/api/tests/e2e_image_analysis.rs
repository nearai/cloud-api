//! E2E tests for image analysis endpoint (/v1/images/analyses)

mod common;
use api::models::BatchUpdateModelApiRequest;
use common::*;

/// Helper to setup a vision model for testing
async fn setup_vision_model(server: &axum_test::TestServer) -> String {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "Qwen/Qwen3-VL-30B-A3B-Instruct".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 5000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 15000000,
                "currency": "USD"
            },
            "costPerImage": {
                "amount": 0,
                "currency": "USD"
            },
            "modelDisplayName": "Qwen3 VL 30B",
            "modelDescription": "Qwen3 Vision Language model",
            "contextLength": 32768,
            "verifiable": true,
            "isActive": true,
            "inputModalities": ["text", "image"],
            "outputModalities": ["text"]
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    "Qwen/Qwen3-VL-30B-A3B-Instruct".to_string()
}

/// Test image analysis with a vision model
#[tokio::test]
async fn test_image_analysis_with_vision_model() {
    let (server, _guard) = setup_test_server().await;
    let vision_model = setup_vision_model(&server).await;

    // Setup org and API key
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // 1x1 red pixel PNG (base64)
    let base64_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

    let response = server
        .post("/v1/images/analyses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": vision_model,
            "image": format!("data:image/png;base64,{}", base64_image),
            "prompt": "What color is this image?",
            "max_tokens": 100
        }))
        .await;

    println!("Response status: {}", response.status_code());
    let body = response.text();
    println!("Response body: {}", body);

    // Verify successful response
    assert_eq!(response.status_code(), 200);

    let data: serde_json::Value = serde_json::from_str(&body).expect("Valid JSON");
    assert_eq!(data["object"], "image.analysis");
    assert_eq!(data["model"], vision_model);
    assert!(data["analysis"].is_string());
    assert!(!data["analysis"].as_str().unwrap().is_empty());
    assert!(data["usage"].is_object());
    assert!(data["usage"]["total_tokens"].as_i64().unwrap() > 0);
}

/// Test image analysis rejects non-vision models
#[tokio::test]
async fn test_image_analysis_rejects_non_vision_model() {
    let (server, _guard) = setup_test_server().await;

    // Setup a text-only model
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        "meta-llama/Llama-2-7b-hf".to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 5000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 15000000,
                "currency": "USD"
            },
            "costPerImage": {
                "amount": 0,
                "currency": "USD"
            },
            "modelDisplayName": "Llama 2 7B",
            "modelDescription": "Meta Llama 2 7B (text-only)",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true,
            "inputModalities": ["text"],
            "outputModalities": ["text"]
        }))
        .unwrap(),
    );
    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have updated 1 model");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let text_model = "meta-llama/Llama-2-7b-hf";
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/images/analyses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": text_model,
            "image": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==",
            "prompt": "What is this?"
        }))
        .await;

    println!("Response status: {}", response.status_code());
    assert_eq!(response.status_code(), 400);

    let error: serde_json::Value = serde_json::from_str(&response.text()).expect("Valid JSON");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not support image analysis"));
}

/// Test image analysis validates request parameters
#[tokio::test]
async fn test_image_analysis_validation() {
    let (server, _guard) = setup_test_server().await;
    let vision_model = setup_vision_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test missing prompt
    let response = server
        .post("/v1/images/analyses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": vision_model,
            "image": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==",
            "prompt": ""
        }))
        .await;

    assert_eq!(response.status_code(), 400);

    // Test invalid image format
    let response = server
        .post("/v1/images/analyses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": vision_model,
            "image": "not-a-data-url",
            "prompt": "What is this?"
        }))
        .await;

    assert_eq!(response.status_code(), 400);
}

/// Test image analysis with file ID (future extension)
#[tokio::test]
async fn test_image_analysis_with_file_id() {
    let (server, _guard) = setup_test_server().await;
    let vision_model = setup_vision_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Test with file ID format - should accept the format
    let response = server
        .post("/v1/images/analyses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "model": vision_model,
            "image": {
                "file_id": "file-12345"
            },
            "prompt": "What is this?"
        }))
        .await;

    // With mocks, this should succeed (format is accepted)
    assert_eq!(response.status_code(), 200);

    let data: serde_json::Value = serde_json::from_str(&response.text()).expect("Valid JSON");
    assert_eq!(data["object"], "image.analysis");
    assert!(data["analysis"].is_string());
}

/// Test image analysis with multipart file upload
#[tokio::test]
async fn test_image_analysis_multipart_upload() {
    let (server, _guard) = setup_test_server().await;
    let vision_model = setup_vision_model(&server).await;
    let org = setup_org_with_credits(&server, 100_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // 1x1 red pixel PNG (binary)
    let png_binary = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
        0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
        0x99, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
        0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let response = server
        .post("/v1/images/analyses/upload")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("model", &vision_model)
                .add_text("prompt", "What color is this image?")
                .add_text("max_tokens", "100")
                .add_part(
                    "image",
                    axum_test::multipart::Part::bytes(png_binary)
                        .file_name("test.png")
                        .mime_type("image/png"),
                ),
        )
        .await;

    println!("Response status: {}", response.status_code());
    let body = response.text();
    println!("Response body: {}", body);

    assert_eq!(response.status_code(), 200);

    let data: serde_json::Value = serde_json::from_str(&body).expect("Valid JSON");
    assert_eq!(data["object"], "image.analysis");
    assert_eq!(data["model"], vision_model);
    assert!(data["analysis"].is_string());
    assert!(!data["analysis"].as_str().unwrap().is_empty());
    assert!(data["usage"].is_object());
    assert!(data["usage"]["total_tokens"].as_i64().unwrap() > 0);
}
