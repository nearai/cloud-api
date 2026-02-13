//! E2E tests for MCP (Model Context Protocol) tool support in the responses API.
//!
//! This test simulates a realistic multi-turn conversation with MCP tools:
//! 1. First request: discovers tools, returns mcp_list_tools
//! 2. Second request: client sends cached tools, LLM requests tool call, approval required
//! 3. Third request: client sends approval, tool executes, LLM produces final response

mod common;

use common::*;
use services::responses::models::McpDiscoveredTool;
use services::responses::tools::{MockMcpClient, MockMcpClientFactory};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[tokio::test]
async fn test_mcp_multi_turn_conversation_with_approval() {
    // Track list_tools calls to verify caching works
    let list_tools_call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = list_tools_call_count.clone();

    // Create mock factory
    let mut mock_factory = MockMcpClientFactory::new();
    mock_factory
        .expect_create_client()
        .withf(|url: &str, _| url == "https://example.com/mcp")
        .returning(move |_, _| {
            let count = call_count_clone.clone();
            let mut client = MockMcpClient::new();

            // list_tools increments counter
            client.expect_list_tools().returning(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(vec![McpDiscoveredTool {
                    name: "get_weather".to_string(),
                    description: Some("Get weather for a location".to_string()),
                    input_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    })),
                    annotations: None,
                }])
            });

            // call_tool returns weather data
            client
                .expect_call_tool()
                .withf(|name: &str, _| name == "get_weather")
                .returning(|_, args| {
                    let location = args
                        .get("location")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    Ok(format!("Weather in {}: Sunny, 72°F", location))
                });

            Ok(Box::new(client) as Box<dyn services::responses::tools::mcp::McpClient>)
        });

    let mcp_factory = Arc::new(mock_factory);
    let (server, _pool, mock, _guard) = setup_test_server_with_mcp_factory(mcp_factory).await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "MCP Multi-turn Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    let mcp_tool = serde_json::json!({
        "type": "mcp",
        "server_label": "weather_server",
        "server_url": "https://example.com/mcp",
        "require_approval": "always"
    });

    // ========================================
    // Turn 1: Tool discovery
    // ========================================
    println!("Turn 1: Tool discovery...");

    use common::mock_prompts;
    let turn1_prompt = mock_prompts::build_prompt("What can you help me with?");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(turn1_prompt))
        .respond_with(inference_providers::mock::ResponseTemplate::new(
            "I can check the weather for you using the get_weather tool. What location would you like to know about?",
        ))
        .await;

    let resp1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": "What can you help me with?",
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;

    assert_eq!(resp1.status_code(), 200, "Turn 1 failed: {}", resp1.text());
    let resp1_obj = resp1.json::<api::models::ResponseObject>();
    assert_eq!(resp1_obj.status, api::models::ResponseStatus::Completed);
    assert_eq!(
        list_tools_call_count.load(Ordering::SeqCst),
        1,
        "list_tools should be called once"
    );

    // Extract mcp_list_tools for caching
    let mcp_list_tools = resp1_obj
        .output
        .iter()
        .find(|item| matches!(item, api::models::ResponseOutputItem::McpListTools { .. }))
        .expect("Turn 1 should return mcp_list_tools");

    // Verify the discovered tools
    if let api::models::ResponseOutputItem::McpListTools { tools, .. } = mcp_list_tools {
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
    } else {
        panic!("Expected McpListTools variant");
    }

    println!("  ✓ Discovered 1 tool: get_weather");

    // ========================================
    // Turn 2: Tool call requires approval
    // ========================================
    println!("Turn 2: Tool invocation with cached tools (requires approval)...");

    // LLM will request a tool call
    let turn1_user = "What can you help me with?";
    let turn1_assistant = "I can check the weather for you using the get_weather tool. What location would you like to know about?";
    let turn2_user = "What's the weather in San Francisco?";

    // LLM requests tool call - but approval is required so tool won't execute yet
    let turn2_prompt = mock_prompts::build_prompt(&format!(
        "{} {} {}",
        turn1_user, turn1_assistant, turn2_user
    ));
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn2_prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
            inference_providers::mock::ToolCall::new(
                "weather_server:get_weather",
                r#"{"location": "San Francisco"}"#,
            ),
        ]),
    )
    .await;

    let resp2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": [
                mcp_list_tools,
                {"type": "message", "role": "user", "content": "What's the weather in San Francisco?"}
            ],
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;

    assert_eq!(resp2.status_code(), 200, "Turn 2 failed: {}", resp2.text());
    let resp2_obj = resp2.json::<api::models::ResponseObject>();

    // Response should be incomplete - waiting for approval
    assert_eq!(
        resp2_obj.status,
        api::models::ResponseStatus::Incomplete,
        "Turn 2 should be incomplete (waiting for approval)"
    );

    // Verify list_tools was NOT called again (caching worked)
    assert_eq!(
        list_tools_call_count.load(Ordering::SeqCst),
        1,
        "list_tools should NOT be called again when cached"
    );
    println!("  ✓ list_tools was not called (cache hit)");

    // Extract mcp_approval_request from output
    let (approval_request_id, tool_name, arguments) = resp2_obj
        .output
        .iter()
        .find_map(|item| {
            if let api::models::ResponseOutputItem::McpApprovalRequest {
                id,
                name,
                arguments,
                ..
            } = item
            {
                Some((id.clone(), name.clone(), arguments.clone()))
            } else {
                None
            }
        })
        .expect("Turn 2 should return mcp_approval_request");

    assert_eq!(tool_name, "get_weather");
    assert!(arguments.contains("San Francisco"));
    println!(
        "  ✓ Received approval request: {} for tool '{}'",
        approval_request_id, tool_name
    );

    // ========================================
    // Turn 3: Approve and execute tool
    // ========================================
    println!("Turn 3: Approving tool call and getting result...");

    let tool_result = "Weather in San Francisco: Sunny, 72°F";

    // After approval, tool executes and LLM produces final response
    // The tool result is appended as a "tool" role message
    let turn3_with_tool_result_prompt = mock_prompts::build_prompt(&format!(
        "{} {} {} {}",
        turn1_user, turn1_assistant, turn2_user, tool_result
    ));
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn3_with_tool_result_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "The weather in San Francisco is currently sunny and 72°F. Perfect weather for outdoor activities!",
    ))
    .await;

    let resp3 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "previous_response_id": resp2_obj.id,
            "input": [
                mcp_list_tools,
                {
                    "type": "mcp_approval_response",
                    "approval_request_id": approval_request_id,
                    "approve": true
                }
            ],
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;

    assert_eq!(resp3.status_code(), 200, "Turn 3 failed: {}", resp3.text());
    let resp3_obj = resp3.json::<api::models::ResponseObject>();

    // Verify the final response contains the weather information
    let final_message = resp3_obj
        .output
        .iter()
        .find(|item| matches!(item, api::models::ResponseOutputItem::Message { .. }));
    assert!(
        final_message.is_some(),
        "Turn 3 should have a message output. Got: {:?}",
        resp3_obj.output
    );

    // Extract text from the message content
    let text =
        if let api::models::ResponseOutputItem::Message { content, .. } = final_message.unwrap() {
            assert!(!content.is_empty(), "message content should not be empty");
            match &content[0] {
                api::models::ResponseContentItem::OutputText { text, .. } => text.clone(),
                _ => panic!("Expected OutputText content"),
            }
        } else {
            panic!("Expected Message variant");
        };

    assert!(
        text.contains("San Francisco") || text.contains("72°F") || text.contains("sunny"),
        "Final response should reference weather. Got: {}",
        text
    );
    println!("  ✓ LLM produced final response: {}", text);

    // Verify the conversation completed successfully
    assert_eq!(resp3_obj.status, api::models::ResponseStatus::Completed);

    println!("  ✓ Response completed successfully");
    println!("\n✅ MCP multi-turn conversation with approval test passed!");
}
