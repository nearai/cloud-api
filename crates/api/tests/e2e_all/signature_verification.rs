// Import common test utilities

use crate::common::*;

use api::models::BatchUpdateModelApiRequest;
use bytes::Bytes;
use inference_providers::StreamChunk;

fn first_stream_chat_id(response_text: &str) -> String {
    response_text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| data.trim() != "[DONE]")
        .find_map(|data| match serde_json::from_str::<StreamChunk>(data) {
            Ok(StreamChunk::Chat(chunk)) => Some(chunk.id),
            Ok(StreamChunk::Text(chunk)) => Some(chunk.id),
            _ => None,
        })
        .expect("stream should include a chat completion id")
}

// ============================================
// Streaming Signature Verification Tests
// ============================================

#[tokio::test]
async fn test_raw_stream_without_upstream_done_uses_gateway_signature() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router, _pool, mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Keep all generated chunks but suppress the mock's terminal `[DONE]`.
    // This models a clean upstream EOF for which Cloud API must mint the
    // client-visible terminator itself.
    mock.set_default_response(
        inference_providers::mock::ResponseTemplate::new("one two")
            .with_disconnect_after(usize::MAX),
    )
    .await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": true,
        "stream_options": { "continuous_usage_stats": true },
        "nonce": 901
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(request_json.clone()))
        .expect("request should build");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let mut body = response.into_body();
    let mut received = Vec::new();
    let mut saw_done = false;
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("stream frame should not error");
        let Some(data) = frame.data_ref() else {
            continue;
        };
        received.extend_from_slice(data);
        if String::from_utf8_lossy(&received).contains("data: [DONE]") {
            saw_done = true;
            break;
        }
    }
    let response_text = String::from_utf8(received).expect("SSE body should be UTF-8");
    assert!(saw_done, "Cloud API should append [DONE]: {response_text}");
    assert!(response_text.ends_with("data: [DONE]\n\n"));
    let chat_id = first_stream_chat_id(&response_text);

    // The write must finish before the synthesized marker reaches the client.
    let signature_request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/v1/signature/{chat_id}?signing_algo=ecdsa"))
        .header("Authorization", format!("Bearer {api_key}"))
        .body(axum::body::Body::empty())
        .expect("signature request should build");
    let signature_response = router
        .clone()
        .oneshot(signature_request)
        .await
        .expect("router should serve signature request");
    let signature_status = signature_response.status();
    let signature_bytes = signature_response
        .into_body()
        .collect()
        .await
        .expect("signature body should collect")
        .to_bytes();
    assert_eq!(
        signature_status,
        axum::http::StatusCode::OK,
        "Gateway signature must be available with [DONE]: {}",
        String::from_utf8_lossy(&signature_bytes)
    );
    let signature: serde_json::Value =
        serde_json::from_slice(&signature_bytes).expect("signature response should be JSON");
    assert_eq!(signature["signature_kind"], "gateway");
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(&request_json),
            compute_sha256(&response_text)
        )
    );
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);

    while let Some(frame) = body.frame().await {
        let frame = frame.expect("trailing frame should not error");
        if let Some(data) = frame.data_ref() {
            assert!(
                data.is_empty(),
                "no bytes may follow [DONE]: {:?}",
                String::from_utf8_lossy(data)
            );
        }
    }
}

#[tokio::test]
async fn test_e2ee_raw_stream_without_upstream_done_does_not_create_gateway_signature() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router, _pool, mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock.set_default_response(
        inference_providers::mock::ResponseTemplate::new("one two")
            .with_disconnect_after(usize::MAX),
    )
    .await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": true,
        "stream_options": { "continuous_usage_stats": true },
        "nonce": 903
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("X-Signing-Algo", "ecdsa")
        .body(axum::body::Body::from(request_json))
        .expect("request should build");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let response_bytes = response
        .into_body()
        .collect()
        .await
        .expect("stream body should collect")
        .to_bytes();
    let response_text =
        String::from_utf8(response_bytes.to_vec()).expect("SSE body should be UTF-8");
    assert!(response_text.ends_with("data: [DONE]\n\n"));
    let chat_id = first_stream_chat_id(&response_text);

    let signature_request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/v1/signature/{chat_id}?signing_algo=ecdsa"))
        .header("Authorization", format!("Bearer {api_key}"))
        .body(axum::body::Body::empty())
        .expect("signature request should build");
    let signature_response = router
        .oneshot(signature_request)
        .await
        .expect("router should serve signature request");
    assert_eq!(
        signature_response.status(),
        axum::http::StatusCode::NOT_FOUND,
        "E2EE streams without an upstream [DONE] must not switch to a gateway signature"
    );
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);
}

#[tokio::test]
async fn test_raw_stream_with_upstream_done_retains_provider_signature() {
    let (server, _router, _pool, _mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": true,
        "stream_options": { "continuous_usage_stats": true },
        "nonce": 902
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response_text = response.text();
    assert!(response_text.ends_with("data: [DONE]\n\n"));
    let chat_id = first_stream_chat_id(&response_text);
    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature_response.status_code(),
        200,
        "provider signature should be available: {}",
        signature_response.text()
    );
    let signature = signature_response.json::<serde_json::Value>();
    assert_eq!(signature["signature_kind"], "provider_tee");
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(&request_json),
            compute_sha256(&response_text)
        )
    );
}

#[tokio::test]
async fn test_raw_provider_signature_is_available_when_done_is_emitted() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router, _pool, mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": true,
        "stream_options": { "continuous_usage_stats": true },
        "nonce": 904
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(request_json.clone()))
        .expect("request should build");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let mut body = response.into_body();
    let mut received = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("stream frame should not error");
        let Some(data) = frame.data_ref() else {
            continue;
        };
        received.extend_from_slice(data);
        if String::from_utf8_lossy(&received).contains("data: [DONE]") {
            break;
        }
    }
    let response_text = String::from_utf8(received).expect("SSE body should be UTF-8");
    assert!(response_text.ends_with("data: [DONE]\n\n"));
    let chat_id = first_stream_chat_id(&response_text);

    let signature_request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/v1/signature/{chat_id}?signing_algo=ecdsa"))
        .header("Authorization", format!("Bearer {api_key}"))
        .body(axum::body::Body::empty())
        .expect("signature request should build");
    let signature_response = router
        .clone()
        .oneshot(signature_request)
        .await
        .expect("router should serve signature request");
    let signature_status = signature_response.status();
    let signature_bytes = signature_response
        .into_body()
        .collect()
        .await
        .expect("signature body should collect")
        .to_bytes();
    assert_eq!(
        signature_status,
        axum::http::StatusCode::OK,
        "provider signature must be available with [DONE]: {}",
        String::from_utf8_lossy(&signature_bytes)
    );
    let signature: serde_json::Value =
        serde_json::from_slice(&signature_bytes).expect("signature response should be JSON");
    assert_eq!(signature["signature_kind"], "provider_tee");
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(&request_json),
            compute_sha256(&response_text)
        )
    );
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);
}

#[tokio::test]
async fn test_raw_stream_error_does_not_store_a_signature() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock.set_default_response(
        inference_providers::mock::ResponseTemplate::new("partial output").with_stream_error_after(
            1,
            inference_providers::CompletionError::HttpError {
                status_code: 503,
                message: "upstream stream failed".to_string(),
                is_external: false,
            },
        ),
    )
    .await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "messages": [{ "role": "user", "content": "Respond with two words." }],
        "stream": true,
        "stream_options": { "continuous_usage_stats": true },
        "nonce": 903
    });
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response_text = response.text();
    assert!(response_text.contains("error"));
    let chat_id = first_stream_chat_id(&response_text);
    let client = database
        .pool()
        .get()
        .await
        .expect("database should connect");
    let row = client
        .query_one(
            "SELECT COUNT(*) FROM chat_signatures WHERE chat_id = $1",
            &[&chat_id],
        )
        .await
        .expect("signature count query should succeed");
    let signature_count: i64 = row.get(0);
    assert_eq!(
        signature_count, 0,
        "error streams must not store a signature"
    );
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);
}

#[tokio::test]
async fn test_legacy_completion_gateway_signature_hashes_public_json() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "prompt": "Respond with only two words.",
        "max_tokens": 16
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let response = server
        .post("/v1/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .content_type("application/json")
        .bytes(Bytes::from(request_json.clone()))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let response_text = response.text();
    let completion: serde_json::Value =
        serde_json::from_str(&response_text).expect("legacy response should be JSON");
    let chat_id = completion["id"]
        .as_str()
        .expect("legacy response should include an id");

    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature_response.status_code(),
        200,
        "gateway signature should be available: {}",
        signature_response.text()
    );
    let signature = signature_response.json::<serde_json::Value>();
    assert_eq!(signature["signature_kind"], "gateway");
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(&request_json),
            compute_sha256(&response_text)
        )
    );
}

#[tokio::test]
async fn test_legacy_stream_gateway_signature_is_ready_at_done() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router) = setup_test_server_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request_body = serde_json::json!({
        "model": E2E_QWEN_MODEL_NAME,
        "prompt": "Respond with only two words.",
        "max_tokens": 16,
        "stream": true
    });
    let request_json = serde_json::to_string(&request_body).expect("request should serialize");
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(request_json.clone()))
        .expect("request should build");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let mut body = response.into_body();
    let mut received = Vec::new();
    let mut saw_done = false;
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("stream frame should not error");
        let Some(data) = frame.data_ref() else {
            continue;
        };
        received.extend_from_slice(data);
        if String::from_utf8_lossy(&received).contains("data: [DONE]") {
            saw_done = true;
            break;
        }
    }
    let response_text = String::from_utf8(received).expect("SSE body should be UTF-8");
    assert!(
        saw_done,
        "legacy stream should end with [DONE]: {response_text}"
    );

    let chat_id = response_text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| data.trim() != "[DONE]")
        .find_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .and_then(|chunk| chunk["id"].as_str().map(ToOwned::to_owned))
        .expect("legacy stream should include an id");

    let signature_request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/v1/signature/{chat_id}?signing_algo=ecdsa"))
        .header("Authorization", format!("Bearer {api_key}"))
        .body(axum::body::Body::empty())
        .expect("signature request should build");
    let signature_response = router
        .clone()
        .oneshot(signature_request)
        .await
        .expect("router should serve signature request");
    let signature_status = signature_response.status();
    let signature_bytes = signature_response
        .into_body()
        .collect()
        .await
        .expect("signature body should collect")
        .to_bytes();
    assert_eq!(
        signature_status,
        axum::http::StatusCode::OK,
        "gateway signature must be available the instant [DONE] is decoded: {}",
        String::from_utf8_lossy(&signature_bytes)
    );
    let signature: serde_json::Value =
        serde_json::from_slice(&signature_bytes).expect("signature response should be JSON");
    assert_eq!(signature["signature_kind"], "gateway");
    assert_eq!(
        signature["text"],
        format!(
            "{}:{}",
            compute_sha256(&request_json),
            compute_sha256(&response_text)
        )
    );

    while let Some(frame) = body.frame().await {
        let frame = frame.expect("trailing frame should not error");
        if let Some(data) = frame.data_ref() {
            assert!(
                data.is_empty(),
                "no bytes may follow [DONE]: {:?}",
                String::from_utf8_lossy(data)
            );
        }
    }
}

#[tokio::test]
async fn test_dropping_alias_stream_releases_signature_routing_pin() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router, _pool, mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let alias = format!("test-signature-alias-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        E2E_QWEN_MODEL_NAME.to_string(),
        serde_json::from_value(serde_json::json!({ "aliases": [alias] }))
            .expect("alias update should deserialize"),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({
                "model": alias,
                "messages": [{ "role": "user", "content": "Respond with two words." }],
                "stream": true,
                "stream_options": { "continuous_usage_stats": true },
                "nonce": 905
            })
            .to_string(),
        ))
        .expect("request should build");
    let response = router
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let mut body = response.into_body();
    let frame = body
        .frame()
        .await
        .expect("stream should yield a first frame")
        .expect("first frame should not error");
    let first_bytes = frame.data_ref().expect("first frame should contain data");
    let first_response = String::from_utf8(first_bytes.to_vec()).expect("SSE should be UTF-8");
    let chat_id = first_stream_chat_id(&first_response);

    drop(body);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if mock.unpinned_chat_ids() == vec![chat_id.clone()] {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelling an alias stream should release its signature routing pin");
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);
}

#[tokio::test]
async fn test_dropping_legacy_stream_releases_signature_routing_pin() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router, _pool, mock, _database) = setup_test_server_with_pool_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({
                "model": E2E_QWEN_MODEL_NAME,
                "prompt": "Respond with two words.",
                "stream": true,
                "nonce": 906
            })
            .to_string(),
        ))
        .expect("request should build");
    let response = router
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let mut body = response.into_body();
    let frame = body
        .frame()
        .await
        .expect("stream should yield a first frame")
        .expect("first frame should not error");
    let first_bytes = frame.data_ref().expect("first frame should contain data");
    let first_response = String::from_utf8(first_bytes.to_vec()).expect("SSE should be UTF-8");
    let chat_id = first_stream_chat_id(&first_response);

    drop(body);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if mock.unpinned_chat_ids() == vec![chat_id.clone()] {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelling a legacy stream should release its signature routing pin");
    assert_eq!(mock.unpinned_chat_ids(), vec![chat_id]);
}

#[tokio::test]
async fn test_streaming_chat_completion_signature_verification() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    println!("Created organization: {}", org.id);

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Use a simple, consistent model for testing
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    // Step 1 & 2: Construct request body with streaming enabled
    let request_body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Respond with only two words."
            }
        ],
        "stream": true,
        "model": model_name,
        "nonce": 42
    });

    println!("\n=== Request Body ===");
    println!("{}", serde_json::to_string_pretty(&request_body).unwrap());

    // Step 3: Compute expected request hash
    let request_json = serde_json::to_string(&request_body).expect("Failed to serialize request");
    let expected_request_hash = compute_sha256(&request_json);
    println!("\n=== Expected Request Hash ===");
    println!("Request JSON: {request_json}");
    println!("Expected hash: {expected_request_hash}");

    // Step 4: Make streaming request and capture raw response
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;

    println!("\n=== Response Status ===");
    println!("Status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request should succeed"
    );

    // Capture the complete raw response text (SSE format)
    let response_text = response.text();
    println!("=== Raw Streaming Response ===");
    println!("{response_text}");

    // Step 5: Parse streaming response to extract chat_id and verify structure
    let mut chat_id: Option<String> = None;
    let mut content = String::new();

    println!("=== Parsing SSE Stream ===");
    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                println!("Stream completed with [DONE]");
                break;
            }

            if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
                // Extract chat_id from first chunk
                if chat_id.is_none() {
                    chat_id = Some(chat_chunk.id.clone());
                    println!("Extracted chat_id: {}", chat_chunk.id);
                }

                // Accumulate content
                if let Some(choice) = chat_chunk.choices.first() {
                    if let Some(delta) = &choice.delta {
                        if let Some(delta_content) = &delta.content {
                            content.push_str(delta_content.as_str());
                        }
                    }
                }
            }
        }
    }

    let chat_id = chat_id.expect("Should have extracted chat_id from stream");
    println!("Accumulated content: '{content}'");
    assert!(!content.is_empty(), "Should have received some content");

    // Step 6: Compute expected response hash from the complete raw response
    let expected_response_hash = compute_sha256(&response_text);
    println!("\n=== Expected Response Hash ===");
    println!("Expected hash: {expected_response_hash}");

    // Wait for signature to be stored asynchronously
    println!("\n=== Waiting for Signature Storage ===");
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Step 7: Query signature API
    println!("\n=== Querying Signature API ===");
    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?model={model_name}&signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    println!("Signature API status: {}", signature_response.status_code());
    assert_eq!(
        signature_response.status_code(),
        200,
        "Signature API should return successfully"
    );

    let signature_json = signature_response.json::<serde_json::Value>();
    println!(
        "Signature response: {}",
        serde_json::to_string_pretty(&signature_json).unwrap()
    );

    // Step 8: Parse signature text field (format: "request_hash:response_hash")
    let signature_text = signature_json
        .get("text")
        .and_then(|v| v.as_str())
        .expect("Signature response should have 'text' field");

    println!("\n=== Parsing Signature Text ===");
    println!("Signature text: {signature_text}");

    let hash_parts: Vec<&str> = signature_text.split(':').collect();
    assert_eq!(
        hash_parts.len(),
        2,
        "Signature text should contain two hashes separated by ':'"
    );

    let actual_request_hash = hash_parts[0];
    let actual_response_hash = hash_parts[1];

    println!("Actual request hash:  {actual_request_hash}");
    println!("Actual response hash: {actual_response_hash}");

    // Step 9: Critical Assertions - These will FAIL with the current bug
    println!("\n=== Hash Verification ===");

    println!("\nRequest Hash Comparison:");
    println!("  Expected: {expected_request_hash}");
    println!("  Actual:   {actual_request_hash}");

    assert_eq!(
        expected_request_hash, actual_request_hash,
        "\n\n❌ REQUEST HASH MISMATCH!\n\
         Expected: {expected_request_hash}\n\
         Actual:   {actual_request_hash}\n\n\
         This means the signature API is not using the correct request body for hashing.\n\
         The signature cannot be verified correctly.\n"
    );

    println!("\nResponse Hash Comparison:");
    println!("  Expected: {expected_response_hash}");
    println!("  Actual:   {actual_response_hash}");

    assert_eq!(
        expected_response_hash, actual_response_hash,
        "\n\n❌ RESPONSE HASH MISMATCH!\n\
         Expected: {expected_response_hash}\n\
         Actual:   {actual_response_hash}\n\n\
         This means the signature API is not using the correct streaming response body for hashing.\n\
         The signature cannot be verified correctly.\n"
    );

    println!("\n✅ All hash verifications passed!");
    println!("The streaming chat completion signatures are correctly computed.");

    // Verify the signature itself is present
    let signature = signature_json
        .get("signature")
        .and_then(|v| v.as_str())
        .expect("Should have signature field");
    assert!(!signature.is_empty(), "Signature should not be empty");
    assert!(
        signature.starts_with("0x"),
        "Signature should be hex-encoded"
    );

    let signing_address = signature_json
        .get("signing_address")
        .and_then(|v| v.as_str())
        .expect("Should have signing_address field");
    assert!(
        !signing_address.is_empty(),
        "Signing address should not be empty"
    );

    let signing_algo = signature_json
        .get("signing_algo")
        .and_then(|v| v.as_str())
        .expect("Should have signing_algo field");
    assert_eq!(signing_algo, "ecdsa", "Should use ECDSA signing algorithm");

    println!("\n=== Test Summary ===");
    println!("✅ Streaming request succeeded");
    println!("✅ Chat completion ID extracted: {chat_id}");
    println!("✅ Content received: {} chars", content.len());
    println!("✅ Signature stored and retrieved");
    println!("✅ Request hash matches: {expected_request_hash}");
    println!("✅ Response hash matches: {expected_response_hash}");
    println!(
        "✅ Signature is present: {}...",
        &signature[..signature.len().min(20)]
    );
    println!("✅ Signing address: {signing_address}");
    println!("✅ Signing algorithm: {signing_algo}");
}

#[tokio::test]
async fn test_streaming_chat_include_usage_signature_hashes_client_bytes() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    let request_body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Respond with only two words."
            }
        ],
        "stream": true,
        "stream_options": {
            "include_usage": true
        },
        "model": model_name,
        "nonce": 43
    });

    let request_json = serde_json::to_string(&request_body).expect("Failed to serialize request");
    let expected_request_hash = compute_sha256(&request_json);

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request_body)
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "Streaming request should succeed: {}",
        response.text()
    );

    let response_text = response.text();
    let expected_response_hash = compute_sha256(&response_text);
    let mut chat_id = None::<String>;
    let mut saw_final_usage = false;
    let mut saw_done = false;

    for line in response_text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            saw_done = true;
            break;
        }
        if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
            if chat_id.is_none() {
                chat_id = Some(chat_chunk.id.clone());
            }
            if chat_chunk.choices.is_empty() && chat_chunk.usage.is_some() {
                saw_final_usage = true;
            }
        }
    }

    let chat_id = chat_id.expect("Should have extracted chat_id from stream");
    assert!(
        saw_final_usage,
        "include_usage=true stream should include a final usage chunk: {response_text}"
    );
    assert!(saw_done, "stream should end with [DONE]: {response_text}");

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let signature_response = server
        .get(format!("/v1/signature/{chat_id}?model={model_name}&signing_algo=ecdsa").as_str())
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;
    assert_eq!(
        signature_response.status_code(),
        200,
        "Signature API should return successfully: {}",
        signature_response.text()
    );

    let signature_json = signature_response.json::<serde_json::Value>();
    let signature_text = signature_json
        .get("text")
        .and_then(|v| v.as_str())
        .expect("Signature response should have 'text' field");
    let hash_parts: Vec<&str> = signature_text.split(':').collect();
    assert_eq!(
        hash_parts.len(),
        2,
        "Signature text should contain two hashes separated by ':'"
    );

    assert_eq!(hash_parts[0], expected_request_hash);
    assert_eq!(
        hash_parts[1], expected_response_hash,
        "stored response hash must match the exact include_usage SSE body returned to the client"
    );
}

#[tokio::test]
async fn test_streaming_chat_default_stream_signature_stored_before_done_emitted() {
    // Default streaming (no stream_options) on an attested model takes the
    // usage-strip gateway-signature path. The route must store the gateway
    // signature BEFORE emitting [DONE] (the marker is held back and appended
    // by the end-of-stream tail after the store), so a client that fetches
    // the signature the moment it sees [DONE] must never race the store.
    //
    // `axum_test` buffers the whole response body, which cannot discriminate
    // here: by the time the buffered body is handed back, the tail (and the
    // store) has already run regardless of ordering. Instead, drive the
    // router in-process and poll the SSE body frame-by-frame, issuing the
    // signature GET the instant the [DONE] line is decoded — before polling
    // any further frames. Without the [DONE] holdback the marker arrives in
    // an inline frame before the tail future (which stores the signature) has
    // run, and the GET deterministically misses.
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (server, router) = setup_test_server_and_router().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    let request_body = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Respond with only two words."
            }
        ],
        "stream": true,
        "model": model_name,
        "nonce": 44
    });
    let request_json = serde_json::to_string(&request_body).expect("Failed to serialize request");
    let expected_request_hash = compute_sha256(&request_json);

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(request_json))
        .expect("request should build");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should serve the streaming request");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "Streaming request should succeed"
    );

    // Poll the body frame-by-frame and stop the instant [DONE] is decoded.
    let mut body = response.into_body();
    let mut received: Vec<u8> = Vec::new();
    let mut saw_done = false;
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("stream frame should not error");
        let Some(data) = frame.data_ref() else {
            continue;
        };
        received.extend_from_slice(data);
        if String::from_utf8_lossy(&received).contains("data: [DONE]") {
            saw_done = true;
            break; // deliberately do NOT poll further frames before the GET
        }
    }
    let response_text = String::from_utf8(received).expect("SSE body should be UTF-8");
    assert!(saw_done, "stream should end with [DONE]: {response_text}");
    let expected_response_hash = compute_sha256(&response_text);

    let mut chat_id = None::<String>;
    for line in response_text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            break;
        }
        if let Ok(StreamChunk::Chat(chat_chunk)) = serde_json::from_str::<StreamChunk>(data) {
            if chat_id.is_none() {
                chat_id = Some(chat_chunk.id.clone());
            }
            assert!(
                chat_chunk.usage.is_none(),
                "default stream must not forward populated usage: {data}"
            );
        }
    }
    let chat_id = chat_id.expect("Should have extracted chat_id from stream");

    // The signature GET is issued NOW — after [DONE] was decoded but before
    // the body stream is polled again — so nothing that runs after the
    // [DONE]-bearing frame can have stored the signature yet.
    let signature_request = axum::http::Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/signature/{chat_id}?model={model_name}&signing_algo=ecdsa"
        ))
        .header("Authorization", format!("Bearer {api_key}"))
        .body(axum::body::Body::empty())
        .expect("signature request should build");
    let signature_response = router
        .clone()
        .oneshot(signature_request)
        .await
        .expect("router should serve the signature request");
    let signature_status = signature_response.status();
    let signature_bytes = signature_response
        .into_body()
        .collect()
        .await
        .expect("signature body should collect")
        .to_bytes();
    assert_eq!(
        signature_status,
        axum::http::StatusCode::OK,
        "Signature must be retrievable the instant [DONE] is decoded: {}",
        String::from_utf8_lossy(&signature_bytes)
    );

    let signature_json: serde_json::Value =
        serde_json::from_slice(&signature_bytes).expect("signature response should be JSON");
    let signature_text = signature_json
        .get("text")
        .and_then(|v| v.as_str())
        .expect("Signature response should have 'text' field");
    let hash_parts: Vec<&str> = signature_text.split(':').collect();
    assert_eq!(
        hash_parts.len(),
        2,
        "Signature text should contain two hashes separated by ':'"
    );
    assert_eq!(hash_parts[0], expected_request_hash);
    assert_eq!(
        hash_parts[1], expected_response_hash,
        "stored response hash must match the exact stripped SSE body returned to the client"
    );
    assert_eq!(
        signature_json
            .get("signature_kind")
            .and_then(|v| v.as_str()),
        Some("gateway"),
        "stripped streams are gateway-signed and must say so"
    );

    // Nothing may follow the [DONE]-bearing frame.
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("trailing frame should not error");
        if let Some(data) = frame.data_ref() {
            assert!(
                data.is_empty(),
                "no bytes may follow [DONE]: {:?}",
                String::from_utf8_lossy(data)
            );
        }
    }
}
