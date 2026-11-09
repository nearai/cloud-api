//! E2E boundary coverage for the stateless Responses API.

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use axum::http::Method;
use inference_providers::{mock::MockProvider, responses_raw::ResponsesRawResponse};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

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
async fn stateless_responses_reject_unsupported_tools_before_provider_work() {
    // Request normalization must reject unsupported builtin types before the handler reaches
    // service validation or provider work. A mock makes that boundary
    // observable.
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let rejected_tools = [
        ("file_search", serde_json::json!({"type": "file_search"})),
        (
            "code_interpreter",
            serde_json::json!({"type": "code_interpreter"}),
        ),
        ("computer", serde_json::json!({"type": "computer"})),
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
            "local rejection should identify {tool_type}: {}",
            error.error.message
        );
        assert!(
            mock.last_chat_params().await.is_none(),
            "{tool_type} must be rejected before service/provider work"
        );
    }
}

#[tokio::test]
async fn mcp_tools_are_rejected_before_model_resolution_or_provider_work() {
    // MCP tools are no longer supported. Every transport must answer with the
    // same 400 before model resolution or provider work, wherever the tool is
    // declared. The URL uses the reserved `.invalid` TLD so it cannot point to
    // a real server if this behavior ever regresses.
    let (server, pool, mock, _database) = setup_test_server_with_pool().await;
    let typed_model = setup_qwen_model(&server).await;

    let native_model = format!("openai/mcp-rejection-{}", uuid::Uuid::new_v4());
    let native_calls = Arc::new(AtomicUsize::new(0));
    let calls = native_calls.clone();
    let native_provider = Arc::new(MockProvider::new_accept_all().with_responses_handler(
        move |_body: serde_json::Value| {
            calls.fetch_add(1, Ordering::SeqCst);
            ResponsesRawResponse {
                status: axum::http::StatusCode::OK,
                headers: [(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                )]
                .into_iter()
                .collect(),
                body: Box::pin(futures_util::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"object":"response","status":"completed","output":[]}"#,
                ))])),
            }
        },
    ));
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        native_model.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {"amount": 1000, "currency": "USD"},
            "outputCostPerToken": {"amount": 2000, "currency": "USD"},
            "modelDisplayName": "MCP rejection fixture",
            "modelDescription": "Native Responses test",
            "contextLength": 10000, "maxOutputLength": 1000, "isActive": true, "ownedBy": "openai",
            "verifiable": false, "inputModalities": ["text"], "outputModalities": ["text"]
        }))
        .unwrap(),
    );
    seed_models(&server, batch, get_session_id()).await;
    pool.register_provider(native_model.clone(), native_provider.clone())
        .await;
    let missing_model = format!("missing-model-{}", uuid::Uuid::new_v4());

    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let mcp = serde_json::json!({
        "type": "mcp",
        "server_label": "remote",
        "server_url": "https://mcp.invalid/tools",
        "require_approval": "never"
    });
    let placements = [
        ("top-level", serde_json::json!({"tools": [mcp.clone()]})),
        (
            "alongside a function",
            serde_json::json!({"tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                mcp.clone()
            ]}),
        ),
        (
            "namespace",
            serde_json::json!({"tools": [{
                "type": "namespace", "name": "client", "description": "Client tools.",
                "tools": [mcp.clone()]
            }]}),
        ),
        (
            "additional_tools",
            serde_json::json!({"input": [
                {"type": "additional_tools", "role": "developer", "tools": [mcp.clone()]},
                {"role": "user", "content": "Use the configured tool."}
            ]}),
        ),
    ];

    for model in [&typed_model, &native_model, &missing_model] {
        for stream in [false, true] {
            for (placement, fields) in &placements {
                let mut body = serde_json::json!({
                    "model": model,
                    "input": "Use the configured tool.",
                    "store": false,
                    "stream": stream,
                });
                for (key, value) in fields.as_object().unwrap() {
                    body[key] = value.clone();
                }
                let response = server
                    .post("/v1/responses")
                    .add_header("Authorization", format!("Bearer {api_key}"))
                    .json(&body)
                    .await;

                let case = format!("model={model} stream={stream} placement={placement}");
                assert_eq!(response.status_code(), 400, "{case}: {}", response.text());
                let error = response.json::<api::models::ErrorResponse>();
                assert_eq!(error.error.r#type, "invalid_request_error", "{case}");
                assert_eq!(error.error.param.as_deref(), Some("tools"), "{case}");
                assert_eq!(
                    error.error.message, "MCP tools are no longer supported.",
                    "{case}"
                );
            }
        }
    }

    assert!(
        mock.last_chat_params().await.is_none(),
        "typed MCP requests must not reach the provider"
    );
    assert!(
        native_provider.last_chat_params().await.is_none(),
        "native MCP requests must not fall back to chat"
    );
    assert_eq!(
        native_calls.load(Ordering::SeqCst),
        0,
        "native MCP requests must not reach the provider"
    );
}

#[tokio::test]
async fn stateless_responses_reject_image_output_model_aliases_before_provider_work() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;

    // Configure the capability explicitly instead of relying on the catalog
    // defaults for the model name. Responses must remain a text-completion
    // wrapper and reject image-generation models before asking a provider to
    // do any work.
    let model = format!("responses-image-output-model-{}", uuid::Uuid::new_v4());
    let model_alias = format!("responses-image-output-alias-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model,
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
            "aliases": [model_alias.clone()],
            "inputModalities": ["text"],
            "outputModalities": ["image"]
        }))
        .expect("image-output model configuration must be valid"),
    );
    let updated = seed_models(&server, batch, get_session_id()).await;
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
            "model": model_alias,
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
