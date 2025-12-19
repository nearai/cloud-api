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

/// Get usage records for an organization
async fn get_usage_records_from_db(
    database: &database::Database,
    organization_id: uuid::Uuid,
) -> Vec<(i32, i32)> {
    let pool = database.pool();
    let client = pool.get().await.expect("Failed to get database connection");

    let rows = client
        .query(
            "SELECT input_tokens, output_tokens FROM organization_usage_log WHERE organization_id = $1 ORDER BY created_at DESC",
            &[&organization_id],
        )
        .await
        .expect("Failed to query usage");

    rows.into_iter()
        .map(|row| (row.get("input_tokens"), row.get("output_tokens")))
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
        .find(|(input, output)| *input == 127 && *output == 5);
    assert!(
        main_request_usage.is_some(),
        "Should have usage record with 127 input tokens and 5 output tokens. Found: {:?}",
        usage
    );
}
