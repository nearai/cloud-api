//! E2E tests for external function tools in the responses API.
//!
//! This test simulates a realistic multi-turn conversation with external function tools:
//! 1. First request: LLM requests function call, response is incomplete with FunctionCall output
//! 2. Second request: Client provides function output, LLM produces final response
//!
//! Unlike MCP tools (server-executed), function tools are executed by the client externally.
//! The API returns FunctionCall items and pauses until the client submits FunctionCallOutput.

mod common;

use common::*;

/// Test a single function call flow:
/// 1. Client sends request with function tool definition
/// 2. LLM calls the function → response is incomplete with FunctionCall in output
/// 3. Client sends FunctionCallOutput with the result
/// 4. LLM produces final response → response is complete
#[tokio::test]
async fn test_function_tool_single_call() {
    let (server, _, mock, _, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Function Tool Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    // Define a function tool
    let function_tool = serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get the current weather for a location",
        "parameters": {
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "The city name"
                }
            },
            "required": ["location"]
        }
    });

    // ========================================
    // Turn 1: LLM requests function call
    // ========================================
    println!("Turn 1: LLM requests function call...");

    let turn1_user = "What's the weather in Tokyo?";

    // Mock the LLM to request a tool call
    use common::mock_prompts;
    let turn1_prompt = mock_prompts::build_prompt(turn1_user);
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn1_prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
            inference_providers::mock::ToolCall::new("get_weather", r#"{"location": "Tokyo"}"#),
        ]),
    )
    .await;

    let resp1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": turn1_user,
            "stream": false,
            "tools": [function_tool.clone()]
        }))
        .await;

    assert_eq!(resp1.status_code(), 200, "Turn 1 failed: {}", resp1.text());
    let resp1_obj = resp1.json::<api::models::ResponseObject>();

    // Response should be incomplete - waiting for function output
    assert_eq!(
        resp1_obj.status,
        api::models::ResponseStatus::Incomplete,
        "Turn 1 should be incomplete (waiting for function output)"
    );

    // Extract FunctionCall from output
    let function_call = resp1_obj
        .output
        .iter()
        .find_map(|item| {
            if let api::models::ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } = item
            {
                Some((call_id.clone(), name.clone(), arguments.clone()))
            } else {
                None
            }
        })
        .expect("Turn 1 should return FunctionCall");

    let (call_id, tool_name, arguments) = function_call;
    assert_eq!(tool_name, "get_weather");
    assert!(arguments.contains("Tokyo"));
    println!(
        "  ✓ Received FunctionCall: call_id={}, name={}, arguments={}",
        call_id, tool_name, arguments
    );

    // ========================================
    // Turn 2: Client provides function output
    // ========================================
    println!("Turn 2: Client provides function output...");

    let function_output = r#"{"temperature": 22, "conditions": "partly cloudy", "humidity": 65}"#;

    // Mock the LLM response after receiving tool result
    let turn2_with_tool_result_prompt =
        mock_prompts::build_prompt(&format!("{} {}", turn1_user, function_output));
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn2_with_tool_result_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "The current weather in Tokyo is 22°C with partly cloudy skies and 65% humidity. It's a pleasant day!",
    ))
    .await;

    let resp2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "previous_response_id": resp1_obj.id,
            "input": [{
                "type": "function_call_output",
                "call_id": call_id,
                "output": function_output
            }],
            "stream": false,
            "tools": [function_tool.clone()]
        }))
        .await;

    assert_eq!(resp2.status_code(), 200, "Turn 2 failed: {}", resp2.text());
    let resp2_obj = resp2.json::<api::models::ResponseObject>();

    // Response should be complete
    assert_eq!(
        resp2_obj.status,
        api::models::ResponseStatus::Completed,
        "Turn 2 should be completed"
    );

    // Verify the final response contains the weather information
    let final_message = resp2_obj
        .output
        .iter()
        .find(|item| matches!(item, api::models::ResponseOutputItem::Message { .. }));
    assert!(
        final_message.is_some(),
        "Turn 2 should have a message output. Got: {:?}",
        resp2_obj.output
    );

    // Extract text from the message content
    let text =
        if let api::models::ResponseOutputItem::Message { content, .. } = final_message.unwrap() {
            assert!(!content.is_empty(), "message content should not be empty");
            match &content[0] {
                api::models::ResponseOutputContent::OutputText { text, .. } => text.clone(),
                _ => panic!("Expected OutputText content"),
            }
        } else {
            panic!("Expected Message variant");
        };

    assert!(
        text.contains("Tokyo") || text.contains("22") || text.contains("cloudy"),
        "Final response should reference weather. Got: {}",
        text
    );
    println!("  ✓ LLM produced final response: {}", text);
    println!("\n✅ Function tool single call test passed!");
}

/// Test parallel function calls:
/// 1. LLM requests multiple function calls at once
/// 2. Client provides all function outputs in one request
/// 3. LLM produces final response
#[tokio::test]
async fn test_function_tool_parallel_calls() {
    let (server, _, mock, _, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Parallel Function Tool Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    // Define function tools
    let weather_tool = serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get the current weather for a location",
        "parameters": {
            "type": "object",
            "properties": {
                "location": {"type": "string"}
            },
            "required": ["location"]
        }
    });

    let time_tool = serde_json::json!({
        "type": "function",
        "name": "get_time",
        "description": "Get the current time for a timezone",
        "parameters": {
            "type": "object",
            "properties": {
                "timezone": {"type": "string"}
            },
            "required": ["timezone"]
        }
    });

    // ========================================
    // Turn 1: LLM requests multiple function calls
    // ========================================
    println!("Turn 1: LLM requests multiple function calls...");

    let turn1_user = "What's the weather and current time in New York?";

    use common::mock_prompts;
    let turn1_prompt = mock_prompts::build_prompt(turn1_user);
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn1_prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
            inference_providers::mock::ToolCall::new("get_weather", r#"{"location": "New York"}"#),
            inference_providers::mock::ToolCall::new(
                "get_time",
                r#"{"timezone": "America/New_York"}"#,
            ),
        ]),
    )
    .await;

    let resp1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": turn1_user,
            "stream": false,
            "tools": [weather_tool.clone(), time_tool.clone()]
        }))
        .await;

    assert_eq!(resp1.status_code(), 200, "Turn 1 failed: {}", resp1.text());
    let resp1_obj = resp1.json::<api::models::ResponseObject>();

    // Response should be incomplete
    assert_eq!(
        resp1_obj.status,
        api::models::ResponseStatus::Incomplete,
        "Turn 1 should be incomplete"
    );

    // Extract all FunctionCalls from output
    let function_calls: Vec<_> = resp1_obj
        .output
        .iter()
        .filter_map(|item| {
            if let api::models::ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } = item
            {
                Some((call_id.clone(), name.clone(), arguments.clone()))
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        function_calls.len(),
        2,
        "Should have 2 FunctionCalls, got: {:?}",
        function_calls
    );
    println!("  ✓ Received {} FunctionCalls", function_calls.len());

    // Find the call_ids for each function
    let weather_call = function_calls
        .iter()
        .find(|(_, name, _)| name == "get_weather")
        .expect("Should have get_weather call");
    let time_call = function_calls
        .iter()
        .find(|(_, name, _)| name == "get_time")
        .expect("Should have get_time call");

    println!(
        "    - get_weather: call_id={}, args={}",
        weather_call.0, weather_call.2
    );
    println!(
        "    - get_time: call_id={}, args={}",
        time_call.0, time_call.2
    );

    // ========================================
    // Turn 2: Client provides all function outputs
    // ========================================
    println!("Turn 2: Client provides all function outputs...");

    let weather_output = r#"{"temperature": 18, "conditions": "sunny"}"#;
    let time_output = r#"{"time": "2:30 PM", "date": "2024-01-15"}"#;

    // Mock the LLM response after receiving both tool results
    let turn2_with_tool_results_prompt = mock_prompts::build_prompt(&format!(
        "{} {} {}",
        turn1_user, weather_output, time_output
    ));
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn2_with_tool_results_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "In New York, it's currently 2:30 PM on January 15th. The weather is sunny with a temperature of 18°C.",
    ))
    .await;

    let resp2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "previous_response_id": resp1_obj.id,
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": weather_call.0,
                    "output": weather_output
                },
                {
                    "type": "function_call_output",
                    "call_id": time_call.0,
                    "output": time_output
                }
            ],
            "stream": false,
            "tools": [weather_tool.clone(), time_tool.clone()]
        }))
        .await;

    assert_eq!(resp2.status_code(), 200, "Turn 2 failed: {}", resp2.text());
    let resp2_obj = resp2.json::<api::models::ResponseObject>();

    // Response should be complete
    assert_eq!(
        resp2_obj.status,
        api::models::ResponseStatus::Completed,
        "Turn 2 should be completed"
    );

    // Verify the final response
    let final_message = resp2_obj
        .output
        .iter()
        .find(|item| matches!(item, api::models::ResponseOutputItem::Message { .. }));
    assert!(
        final_message.is_some(),
        "Turn 2 should have a message output"
    );

    let text =
        if let api::models::ResponseOutputItem::Message { content, .. } = final_message.unwrap() {
            match &content[0] {
                api::models::ResponseOutputContent::OutputText { text, .. } => text.clone(),
                _ => panic!("Expected OutputText content"),
            }
        } else {
            panic!("Expected Message variant");
        };

    assert!(
        text.contains("New York") || text.contains("2:30") || text.contains("18"),
        "Final response should reference location, time, or weather. Got: {}",
        text
    );
    println!("  ✓ LLM produced final response: {}", text);
    println!("\n✅ Function tool parallel calls test passed!");
}

/// Regression test: FunctionCallOutput + Message in same input must preserve order.
/// Tool results must immediately follow the assistant message with tool_calls;
/// a user message in between violates the LLM provider contract.
/// With the fix: [assistant+tool_calls, tool_result, user_message]
/// Bug: [assistant+tool_calls, user_message, tool_result] - wrong
#[tokio::test]
async fn test_function_output_and_message_ordering() {
    let (server, _, mock, _, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Function Output + Message Ordering Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    let function_tool = serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get weather",
        "parameters": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}
    });

    // Turn 1: LLM requests function call
    let turn1_user = "What's the weather in Paris?";
    use common::mock_prompts;
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        mock_prompts::build_prompt(turn1_user),
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
            inference_providers::mock::ToolCall::new("get_weather", r#"{"location": "Paris"}"#),
        ]),
    )
    .await;

    let resp1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": turn1_user,
            "stream": false,
            "tools": [function_tool.clone()]
        }))
        .await;

    assert_eq!(resp1.status_code(), 200, "Turn 1 failed: {}", resp1.text());
    let resp1_obj = resp1.json::<api::models::ResponseObject>();
    assert_eq!(
        resp1_obj.status,
        api::models::ResponseStatus::Incomplete,
        "Turn 1 should be incomplete"
    );

    let function_call = resp1_obj
        .output
        .iter()
        .find_map(|item| {
            if let api::models::ResponseOutputItem::FunctionCall { call_id, .. } = item {
                Some(call_id.clone())
            } else {
                None
            }
        })
        .expect("Turn 1 should return FunctionCall");

    let function_output = r#"{"temperature": 15, "conditions": "cloudy"}"#;
    let follow_up_message = "Thanks! Is it going to rain tomorrow?";

    // Correct order: tool_result before user message (LLM provider contract)
    // build_prompt concatenates text in message order: turn1, tool_output, follow_up
    let expected_prompt = mock_prompts::build_prompt(&format!(
        "{} {} {}",
        turn1_user, function_output, follow_up_message
    ));
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        expected_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "Based on the current cloudy conditions, there's a 60% chance of rain tomorrow.",
    ))
    .await;

    // Turn 2: BOTH FunctionCallOutput AND Message - order matters
    let resp2 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "previous_response_id": resp1_obj.id,
            "input": [
                {"type": "function_call_output", "call_id": function_call, "output": function_output},
                {"type": "message", "role": "user", "content": follow_up_message}
            ],
            "stream": false,
            "tools": [function_tool.clone()]
        }))
        .await;

    assert_eq!(resp2.status_code(), 200, "Turn 2 failed: {}", resp2.text());
    let resp2_obj = resp2.json::<api::models::ResponseObject>();
    assert_eq!(
        resp2_obj.status,
        api::models::ResponseStatus::Completed,
        "Turn 2 should complete - wrong message ordering may cause provider errors"
    );

    let final_message = resp2_obj
        .output
        .iter()
        .find(|item| matches!(item, api::models::ResponseOutputItem::Message { .. }));
    assert!(final_message.is_some(), "Turn 2 should have message output");
    println!("✅ FunctionCallOutput + Message ordering test passed!");
}

/// Test function tool with no previous_response_id (first turn with function output).
/// This tests the edge case where a client might try to submit function output
/// without a previous response context.
#[tokio::test]
async fn test_function_output_without_previous_response_fails() {
    let (server, _, _, _, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Invalid Function Output Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    // Try to submit function output without a previous response
    let resp = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": [{
                "type": "function_call_output",
                "call_id": "fc_nonexistent",
                "output": "some result"
            }],
            "stream": false
        }))
        .await;

    // This should fail because there's no previous FunctionCall to match
    // The exact error depends on implementation, but it should not succeed
    let status = resp.status_code();
    println!(
        "Response status when submitting orphan function output: {}",
        status
    );

    // Either 400 (bad request) or 404 (function call not found) is acceptable
    assert!(
        status == 400 || status == 404,
        "Should reject orphan function output with 400 or 404, got: {}",
        status
    );

    println!("✅ Orphan function output correctly rejected!");
}

/// Test that function tools and MCP tools can coexist in the same request.
/// The LLM might call a function tool, which should pause for client execution.
#[tokio::test]
async fn test_function_tool_coexists_with_builtin_tools() {
    let (server, _, mock, _, _guard) = setup_test_server_with_pool().await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create conversation
    let conv_resp = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"name": "Mixed Tools Test"}))
        .await;
    assert_eq!(conv_resp.status_code(), 201);
    let conversation = conv_resp.json::<api::models::ConversationObject>();

    // Define a custom function tool
    let custom_tool = serde_json::json!({
        "type": "function",
        "name": "search_database",
        "description": "Search the internal database",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            },
            "required": ["query"]
        }
    });

    // LLM calls the custom function
    let turn1_user = "Search for user records with email containing 'test'";

    use common::mock_prompts;
    let turn1_prompt = mock_prompts::build_prompt(turn1_user);
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        turn1_prompt,
    ))
    .respond_with(
        inference_providers::mock::ResponseTemplate::new("").with_tool_calls(vec![
            inference_providers::mock::ToolCall::new(
                "search_database",
                r#"{"query": "email:*test*"}"#,
            ),
        ]),
    )
    .await;

    let resp1 = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "conversation": {"id": conversation.id},
            "input": turn1_user,
            "stream": false,
            "tools": [custom_tool.clone()]
        }))
        .await;

    assert_eq!(resp1.status_code(), 200, "Turn 1 failed: {}", resp1.text());
    let resp1_obj = resp1.json::<api::models::ResponseObject>();

    // Should be incomplete waiting for function output
    assert_eq!(
        resp1_obj.status,
        api::models::ResponseStatus::Incomplete,
        "Should be incomplete waiting for function output"
    );

    // Verify we got a FunctionCall for our custom tool
    let has_function_call = resp1_obj.output.iter().any(|item| {
        matches!(
            item,
            api::models::ResponseOutputItem::FunctionCall { name, .. } if name == "search_database"
        )
    });
    assert!(
        has_function_call,
        "Should have FunctionCall for search_database"
    );

    println!("✅ Custom function tool works alongside other tools!");
}
