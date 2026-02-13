// E2E tests for client disconnect scenarios
//
// The mock's with_disconnect_after() simulates a truncated stream (provider ends early),
// which tests that partial responses and usage are correctly saved.
mod common;

use common::*;

/// Get assistant response item for a conversation
async fn get_assistant_item_from_db(
    database: &database::Database,
    conversation_id: &str,
) -> Option<serde_json::Value> {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    let uuid_str = conversation_id
        .strip_prefix("conv_")
        .unwrap_or(conversation_id);
    let conv_uuid = uuid::Uuid::parse_str(uuid_str).expect("Invalid conversation ID");

    let rows = client
        .query(
            "SELECT item FROM response_items WHERE conversation_id = $1 ORDER BY created_at DESC",
            &[&conv_uuid],
        )
        .await
        .expect("Failed to query response_items");

    for row in rows {
        let item: serde_json::Value = row.get("item");
        if item.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            return Some(item);
        }
    }
    None
}

/// Usage record with all relevant fields for testing
#[derive(Debug)]
struct UsageRecord {
    input_tokens: i32,
    output_tokens: i32,
    stop_reason: Option<String>,
    response_id: Option<uuid::Uuid>,
    provider_request_id: Option<String>,
    inference_id: Option<uuid::Uuid>,
}

/// Get usage records for an organization
async fn get_usage_records_from_db(
    database: &database::Database,
    organization_id: uuid::Uuid,
) -> Vec<UsageRecord> {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    let rows = client
        .query(
            "SELECT input_tokens, output_tokens, stop_reason, response_id, provider_request_id, inference_id
             FROM organization_usage_log WHERE organization_id = $1 ORDER BY created_at DESC",
            &[&organization_id],
        )
        .await
        .expect("Failed to query usage");

    rows.into_iter()
        .map(|row| UsageRecord {
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            stop_reason: row.get("stop_reason"),
            response_id: row.get("response_id"),
            provider_request_id: row.get("provider_request_id"),
            inference_id: row.get("inference_id"),
        })
        .collect()
}

/// Extract text from assistant item
fn extract_text(item: &serde_json::Value) -> Option<String> {
    item.get("content")?
        .as_array()?
        .iter()
        .find_map(|c| c.get("text").and_then(|t| t.as_str()))
        .map(|s| s.to_string())
}

#[tokio::test]
async fn test_response_items_saved_on_disconnect() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let org_uuid = uuid::Uuid::parse_str(&org.id).unwrap();

    use common::mock_prompts;

    // Configure mock: 10 words, disconnect after 5
    let full_response = "Machine learning is a fascinating field of artificial intelligence today";
    let prompt = mock_prompts::build_prompt("Tell me about machine learning");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new(full_response).with_disconnect_after(5),
    )
    .await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation: api::models::ConversationObject = conv_resp.json();

    // Make streaming request
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": conversation.id,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Tell me about machine learning"}]}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let _stream = response.text();

    // Wait for async DB writes (stream completion + title generation)
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Verify assistant response saved with partial text
    let item = get_assistant_item_from_db(&database, &conversation.id)
        .await
        .expect("Should have assistant item");
    let text = extract_text(&item).expect("Should have text content");

    assert_eq!(text, "Machine learning is a fascinating");
    assert!(!text.contains("field"));

    // Verify usage recorded with expected token counts
    // Note: Title generation may also record usage, so we find the specific record by input/output
    // Input: 127 tokens = system prompt (~122 words) + user message ("Tell me about machine learning" = 5 words)
    // Output: 5 words before disconnect ("Machine learning is a fascinating")
    let usage = get_usage_records_from_db(&database, org_uuid).await;
    assert_eq!(
        usage.len(),
        2,
        "Should have exactly 2 usage records (main request + title generation). Found: {:?}",
        usage
    );

    // Find the main request's usage (127 input tokens from system prompt + user msg, 5 output tokens)
    let main_request_usage = usage
        .iter()
        .find(|r| r.input_tokens == 127 && r.output_tokens == 5);
    assert!(
        main_request_usage.is_some(),
        "Should have usage record with 127 input tokens and 5 output tokens. Found: {:?}",
        usage
    );

    let main_usage = main_request_usage.unwrap();

    // Note: The mock's with_disconnect_after() truncates the stream but it still ends normally
    // (returns None), so from our perspective it's a "completed" stream. A true client disconnect
    // would occur if the client dropped the connection before consuming all chunks, which would
    // cause stream_completed to remain false when Drop is called.
    assert_eq!(
        main_usage.stop_reason.as_deref(),
        Some("completed"),
        "Stop reason should be 'completed' for stream that ended normally. Found: {:?}",
        main_usage.stop_reason
    );

    // Verify response_id is set (this was called from Responses API)
    assert!(
        main_usage.response_id.is_some(),
        "Response ID should be set for Responses API calls. Found: {:?}",
        main_usage
    );

    // Verify provider_request_id is set (raw ID from provider)
    assert!(
        main_usage.provider_request_id.is_some(),
        "Provider request ID should be set. Found: {:?}",
        main_usage
    );

    // Verify inference_id is set (hashed from provider_request_id)
    assert!(
        main_usage.inference_id.is_some(),
        "Inference ID should be set. Found: {:?}",
        main_usage
    );
}

#[tokio::test]
async fn test_signature_returns_stream_disconnected_on_client_disconnect() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    use common::mock_prompts;

    // Configure mock: 10 words, disconnect after 5
    let full_response = "Machine learning is a fascinating field of artificial intelligence today";
    let prompt = mock_prompts::build_prompt("Tell me about AI");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new(full_response).with_disconnect_after(5),
    )
    .await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation: api::models::ConversationObject = conv_resp.json();

    // Make streaming request
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": conversation.id,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Tell me about AI"}]}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Parse the response to get response_id
    let response_text = response.text();
    let mut response_id: Option<String> = None;
    for line_chunk in response_text.split("\n\n") {
        for line in line_chunk.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(id) = json
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(|id| id.as_str())
                    {
                        response_id = Some(id.to_string());
                    }
                }
            }
        }
    }
    let response_id = response_id.expect("Should have response_id from stream");

    // Wait for async DB writes
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Manually update the usage record to simulate a client disconnect
    // (The mock ends the stream normally, so we need to update the stop_reason manually)
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");
    let response_uuid_str = response_id.strip_prefix("resp_").unwrap_or(&response_id);
    let response_uuid = uuid::Uuid::parse_str(response_uuid_str).expect("Invalid response ID");

    // Delete any signature that might have been stored (to simulate no signature available)
    client
        .execute(
            "DELETE FROM chat_signatures WHERE chat_id = $1",
            &[&response_id],
        )
        .await
        .expect("Failed to delete signature");

    // Update the stop_reason to client_disconnect
    client
        .execute(
            "UPDATE organization_usage_log SET stop_reason = 'client_disconnect' WHERE response_id = $1",
            &[&response_uuid],
        )
        .await
        .expect("Failed to update stop_reason");

    // Now call the signature endpoint - should return 200 with STREAM_DISCONNECTED
    let signature_resp = server
        .get(&format!("/v1/signature/{response_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_resp.status_code(),
        200,
        "Signature endpoint should return 200 for client disconnect. Response: {}",
        signature_resp.text()
    );

    let signature_json: serde_json::Value = signature_resp.json();
    assert_eq!(
        signature_json.get("error_code").and_then(|v| v.as_str()),
        Some("STREAM_DISCONNECTED"),
        "Should have STREAM_DISCONNECTED error_code. Response: {:?}",
        signature_json
    );
    assert_eq!(
        signature_json.get("message").and_then(|v| v.as_str()),
        Some("Verification not available due to disconnection."),
        "Should have expected message. Response: {:?}",
        signature_json
    );
}

#[tokio::test]
async fn test_signature_returns_404_when_not_client_disconnect() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    use common::mock_prompts;

    // Configure mock with normal response
    let full_response = "Hello world";
    let prompt = mock_prompts::build_prompt("Say hello");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        full_response,
    ))
    .await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation: api::models::ConversationObject = conv_resp.json();

    // Make streaming request
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": conversation.id,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Say hello"}]}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Parse the response to get response_id
    let response_text = response.text();
    let mut response_id: Option<String> = None;
    for line_chunk in response_text.split("\n\n") {
        for line in line_chunk.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(id) = json
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(|id| id.as_str())
                    {
                        response_id = Some(id.to_string());
                    }
                }
            }
        }
    }
    let response_id = response_id.expect("Should have response_id from stream");

    // Wait for async DB writes
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Delete any signature that might have been stored
    // The usage record has stop_reason = "completed", so signature should return 404
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    // Ensure no signature exists
    client
        .execute(
            "DELETE FROM chat_signatures WHERE chat_id = $1",
            &[&response_id],
        )
        .await
        .expect("Failed to delete signature");

    // Now call the signature endpoint - should return 404 since stop_reason is "completed"
    let signature_resp = server
        .get(&format!("/v1/signature/{response_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_resp.status_code(),
        404,
        "Signature endpoint should return 404 for completed stream without signature. Response: {}",
        signature_resp.text()
    );
}

#[tokio::test]
async fn test_chat_completion_signature_returns_stream_disconnected_on_client_disconnect() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    use common::mock_prompts;

    // Configure mock: 10 words, disconnect after 5
    let full_response = "Machine learning is a fascinating field of artificial intelligence today";
    let prompt = mock_prompts::build_prompt("Tell me about AI");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new(full_response).with_disconnect_after(5),
    )
    .await;

    // Make streaming chat completion request
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "Tell me about AI"}],
            "stream": true
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Parse the response to get completion id (chatcmpl-xxx format)
    let response_text = response.text();
    let mut completion_id: Option<String> = None;
    for line in response_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                if let Some(id) = json.get("id").and_then(|id| id.as_str()) {
                    completion_id = Some(id.to_string());
                    break;
                }
            }
        }
    }
    let completion_id = completion_id.expect("Should have completion_id from stream");

    // Wait for async DB writes
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Manually update the usage record to simulate a client disconnect
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    // Delete any signature that might have been stored
    client
        .execute(
            "DELETE FROM chat_signatures WHERE chat_id = $1",
            &[&completion_id],
        )
        .await
        .expect("Failed to delete signature");

    // Update the stop_reason to client_disconnect (by provider_request_id for chat completions)
    client
        .execute(
            "UPDATE organization_usage_log SET stop_reason = 'client_disconnect' WHERE provider_request_id = $1",
            &[&completion_id],
        )
        .await
        .expect("Failed to update stop_reason");

    // Now call the signature endpoint - should return 200 with STREAM_DISCONNECTED
    let signature_resp = server
        .get(&format!("/v1/signature/{completion_id}?signing_algo=ecdsa"))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        signature_resp.status_code(),
        200,
        "Signature endpoint should return 200 for client disconnect on chat completion. Response: {}",
        signature_resp.text()
    );

    let signature_json: serde_json::Value = signature_resp.json();
    assert_eq!(
        signature_json.get("error_code").and_then(|v| v.as_str()),
        Some("STREAM_DISCONNECTED"),
        "Should have STREAM_DISCONNECTED error_code. Response: {:?}",
        signature_json
    );
    assert_eq!(
        signature_json.get("message").and_then(|v| v.as_str()),
        Some("Verification not available due to disconnection."),
        "Should have expected message. Response: {:?}",
        signature_json
    );
}
