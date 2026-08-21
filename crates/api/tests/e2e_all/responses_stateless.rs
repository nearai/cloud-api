//! E2E boundary coverage for the stateless Responses API.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use axum::http::Method;
use std::sync::{atomic::Ordering, Arc};

fn assert_response_history_is_gone(response: axum_test::TestResponse) {
    assert_eq!(response.status_code(), 410);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "gone");
    assert!(error.error.message.contains("stateless"));
}

#[tokio::test]
async fn retired_response_history_routes_require_authentication_then_return_gone() {
    let server = setup_test_server().await;

    assert_eq!(
        server.get("/v1/responses/resp_example").await.status_code(),
        401
    );

    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::GET, "/v1/responses/resp_example"),
        (Method::DELETE, "/v1/responses/resp_example"),
        (Method::POST, "/v1/responses/resp_example/cancel"),
        (Method::GET, "/v1/responses/resp_example/input_items"),
    ];

    for (method, path) in routes {
        assert_response_history_is_gone(
            server
                .method(method.clone(), path)
                .add_header("Authorization", format!("Bearer {api_key}"))
                .await,
        );
    }
}

#[tokio::test]
async fn stateless_responses_reject_persistent_fields() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let requests = [
        serde_json::json!({"model": "test-model", "input": "hello", "store": true}),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "conversation": "conv_example"
        }),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "previous_response_id": "resp_example"
        }),
        serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "store": false,
            "background": true
        }),
        serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": [{
                "role": "user",
                "content": [{"type": "input_file", "file_id": "file_example"}]
            }]
        }),
    ];

    for request in requests {
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;

        assert_eq!(
            response.status_code(),
            400,
            "request {request} must be rejected"
        );
        let error = response.json::<api::models::ErrorResponse>();
        assert_eq!(error.error.r#type, "invalid_request_error");
    }
}

#[tokio::test]
async fn stateless_responses_reject_unsupported_tool_types_during_deserialization() {
    // The request enum must reject builtin types before the handler reaches
    // service validation or provider work. A mock makes that boundary
    // observable. The MCP URL uses the reserved `.invalid` TLD so it cannot
    // point to a real third party if this behavior ever regresses.
    let web_search_provider = Arc::new(MockWebSearchProvider::default_results());
    let web_search_call_count = web_search_provider.call_count();
    let (server, _database, mock) =
        setup_test_server_with_search_providers(web_search_provider, None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let rejected_tools = [
        ("web_search", serde_json::json!({"type": "web_search"})),
        (
            "web_context_search",
            serde_json::json!({"type": "web_context_search"}),
        ),
        ("file_search", serde_json::json!({"type": "file_search"})),
        (
            "code_interpreter",
            serde_json::json!({"type": "code_interpreter"}),
        ),
        ("computer", serde_json::json!({"type": "computer"})),
        (
            "mcp",
            serde_json::json!({
                "type": "mcp",
                "server_label": "test",
                "server_url": "https://mcp.invalid/tools",
                "require_approval": "never"
            }),
        ),
    ];

    for (tool_type, tool) in rejected_tools {
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&serde_json::json!({
                "model": model.clone(),
                "input": "Use the configured tool.",
                "store": false,
                "stream": false,
                "tools": [tool],
            }))
            .await;

        assert_eq!(
            response.status_code(),
            400,
            "{tool_type} must be rejected locally: {}",
            response.text()
        );
        let error = response.json::<api::models::ErrorResponse>();
        assert_eq!(
            error.error.r#type, "invalid_request_error",
            "{tool_type} must have a stable client error envelope"
        );
        assert!(
            error.error.message.contains(tool_type),
            "deserialization rejection should identify {tool_type}: {}",
            error.error.message
        );
        assert!(
            error.error.message.contains("function"),
            "the function-only tool enum should reject {tool_type}: {}",
            error.error.message
        );
        assert!(
            mock.last_chat_params().await.is_none(),
            "{tool_type} must be rejected before service/provider work"
        );
    }

    assert_eq!(
        web_search_call_count.load(Ordering::SeqCst),
        0,
        "Responses must not invoke the retained Web Search provider"
    );
}

#[tokio::test]
async fn stateless_responses_reject_image_output_models_before_provider_work() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;

    // Configure the capability explicitly instead of relying on the catalog
    // defaults for the model name. Responses must remain a text-completion
    // wrapper and reject image-generation models before asking a provider to
    // do any work.
    let model = "Qwen/Qwen-Image-2512";
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.to_string(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 0, "currency": "USD" },
            "outputCostPerToken": { "amount": 0, "currency": "USD" },
            "costPerImage": { "amount": 40000000, "currency": "USD" },
            "modelDisplayName": "Responses Image Output Test Model",
            "modelDescription": "Active image-output model for Responses validation",
            "contextLength": 4096,
            "maxOutputLength": 1024,
            "verifiable": true,
            "isActive": true,
            "inputModalities": ["text"],
            "outputModalities": ["image"]
        }))
        .expect("image-output model configuration must be valid"),
    );
    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "image-output model must be active");
    assert_eq!(
        updated[0]
            .metadata
            .architecture
            .as_ref()
            .expect("model architecture must be returned")
            .output_modalities,
        vec!["image".to_string()],
        "test model must advertise image output"
    );
    // Keep this in step with the existing model setup helpers: the test
    // server's model registry updates asynchronously after an admin upsert.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": "Draw a small red square.",
            "store": false,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "image-output models must be rejected before inference: {}",
        response.text()
    );
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(
        error.error.message.to_ascii_lowercase().contains("image"),
        "the client error must explain the unsupported image output: {}",
        error.error.message
    );
    assert!(
        mock.last_chat_params().await.is_none(),
        "an image-output Responses request must be rejected before chat completion"
    );
}

#[tokio::test]
async fn stateless_responses_reject_mcp_approval_continuations_before_inference() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "store": false,
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": "mcpr_example",
                "approve": true
            }]
        }))
        .await;

    assert_eq!(response.status_code(), 400, "response: {}", response.text());
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(
        mock.last_chat_params().await.is_none(),
        "MCP approval continuation must be rejected before inference"
    );
}

#[tokio::test]
async fn completed_stateless_response_exposes_gateway_signature_when_persisted() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request = serde_json::json!({
        "model": model,
        "input": "Respond with a short attested answer.",
        "stream": false,
        "store": false
    });
    let expected_request_hash = compute_sha256(
        &serde_json::to_string(&request).expect("serialize stateless Responses request"),
    );

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "stateless Responses request should succeed: {}",
        response.text()
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let response_text = response.text();
    let expected_response_hash = compute_sha256(&response_text);
    let response_json: serde_json::Value =
        serde_json::from_str(&response_text).expect("Responses result must be JSON");
    let response_id = response_json
        .get("id")
        .and_then(|value| value.as_str())
        .expect("completed response must have an ID");
    assert!(response_id.starts_with("resp_"));

    let signature = server
        .get(&format!("/v1/signature/{response_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature.status_code(),
        200,
        "completed response signature should be available when persistence succeeds: {}",
        signature.text()
    );

    let signature: serde_json::Value = signature.json();
    assert_eq!(
        signature
            .get("signature_kind")
            .and_then(|value| value.as_str()),
        Some("gateway")
    );
    assert_eq!(
        signature
            .get("signing_algo")
            .and_then(|value| value.as_str()),
        Some("ecdsa")
    );
    assert!(
        signature
            .get("signature")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()),
        "gateway signature must be present"
    );

    let signed_hashes = signature
        .get("text")
        .and_then(|value| value.as_str())
        .expect("gateway signature must include its signed hashes");
    let (request_hash, response_hash) = signed_hashes
        .split_once(':')
        .expect("gateway signature text must be request_hash:response_hash");
    assert_eq!(request_hash, expected_request_hash);
    assert_eq!(response_hash, expected_response_hash);
}
