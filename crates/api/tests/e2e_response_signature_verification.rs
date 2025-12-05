// Import common test utilities
mod common;

use common::*;

// ============================================
// Response Stream Signature Verification Tests
// ============================================

#[tokio::test]
async fn test_streaming_response_signature_verification() {
    let server = setup_test_server().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    println!("Created organization: {}", org.id);

    let api_key = get_api_key_for_org(&server, org.id).await;

    // Use a simple, consistent model for testing
    let model_name = "Qwen/Qwen3-30B-A3B-Instruct-2507";

    // Step 1: Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": "Test Conversation",
            "description": "A test conversation for signature verification"
        }))
        .await;

    assert_eq!(
        conversation_response.status_code(),
        201,
        "Failed to create conversation"
    );

    let conversation = conversation_response.json::<api::models::ConversationObject>();
    println!("Created conversation: {}", conversation.id);

    // Step 2: Construct request body with streaming enabled
    let request_body = serde_json::json!({
        "conversation": {
            "id": conversation.id,
        },
        "input": "Respond with only two words.",
        "temperature": 0.7,
        "max_output_tokens": 50,
        "stream": true,
        "model": model_name,
        "nonce": 42,
        "signing_algo": "ecdsa"
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
        .post("/v1/responses")
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

    // Step 5: Parse streaming response to extract response_id and verify structure
    let mut response_id: Option<String> = None;
    let mut content = String::new();

    println!("=== Parsing SSE Stream ===");
    for line_chunk in response_text.split("\n\n") {
        if line_chunk.trim().is_empty() {
            continue;
        }

        let mut event_type = "";
        let mut event_data = "";

        for line in line_chunk.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                event_type = event_name;
            } else if let Some(data) = line.strip_prefix("data: ") {
                event_data = data;
            }
        }

        if !event_data.is_empty() {
            if let Ok(event_json) = serde_json::from_str::<serde_json::Value>(event_data) {
                match event_type {
                    "response.created" => {
                        // Extract response_id from the first event
                        if response_id.is_none() {
                            if let Some(response_obj) = event_json.get("response") {
                                if let Some(id) = response_obj.get("id").and_then(|v| v.as_str()) {
                                    response_id = Some(id.to_string());
                                    println!("Extracted response_id: {id}");
                                }
                            }
                        }
                    }
                    "response.output_text.delta" => {
                        // Accumulate content deltas
                        if let Some(delta) = event_json.get("delta").and_then(|v| v.as_str()) {
                            content.push_str(delta);
                        }
                    }
                    "response.completed" => {
                        println!("Stream completed with response.completed event");
                    }
                    _ => {
                        // Other events like response.in_progress, response.output_item.added, etc.
                    }
                }
            }
        }
    }

    let response_id = response_id.expect("Should have extracted response_id from stream");
    println!("Accumulated content: '{content}'");
    assert!(!content.is_empty(), "Should have received some content");

    // Step 6: Compute expected response hash from the complete raw response
    let expected_response_hash = compute_sha256(&response_text);
    println!("\n=== Expected Response Hash ===");
    println!("Expected hash: {expected_response_hash}");

    // Wait for signature to be stored asynchronously
    println!("\n=== Waiting for Signature Storage ===");
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Step 7: Query signature API (using response_id instead of chat_id)
    println!("\n=== Querying Signature API ===");
    let signature_response = server
        .get(format!("/v1/signature/{response_id}?model={model_name}&signing_algo=ecdsa").as_str())
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

    // Step 9: Critical Assertions - Verify hashes match
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
    println!("The streaming response signatures are correctly computed.");

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

    // Step 10: Verify the ECDSA signature cryptographically
    println!("\n=== ECDSA Signature Verification ===");
    println!("Verifying signature for message: {signature_text}");
    println!(
        "Signature: {}",
        &signature[..std::cmp::min(20, signature.len())]
    );
    println!("Signing address: {signing_address}");

    let is_valid = common::verify_ecdsa_signature(signature_text, signature, signing_address);
    assert!(
        is_valid,
        "\n\n❌ ECDSA SIGNATURE VERIFICATION FAILED!\n\
         The signature could not be verified against the message and signing address.\n\
         This means the signature is cryptographically invalid.\n"
    );

    println!("✅ ECDSA signature is cryptographically valid!");
    println!("✅ Recovered public key matches signing address!");

    println!("\n=== Test Summary ===");
    println!("✅ Streaming response request succeeded");
    println!("✅ Response ID extracted: {response_id}");
    println!("✅ Content received: {} chars", content.len());
    println!("✅ Signature stored and retrieved");
    println!("✅ Request hash matches: {expected_request_hash}");
    println!("✅ Response hash matches: {expected_response_hash}");
    println!(
        "✅ Signature is present: {}",
        &signature[..std::cmp::min(20, signature.len())]
    );
    println!("✅ Signing address: {signing_address}");
    println!("✅ Signing algorithm: {signing_algo}");
    println!("✅ ECDSA signature cryptographically verified");
}
