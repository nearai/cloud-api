// Import common test utilities

use crate::common::*;

use inference_providers::StreamChunk;

// ============================================
// Streaming Signature Verification Tests
// ============================================

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
