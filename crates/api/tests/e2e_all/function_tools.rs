//! E2E coverage for client-managed function tools on stateless Responses.

use crate::common::*;
use inference_providers::{
    mock::{RequestMatcher, ResponseTemplate, ToolCall},
    MessageRole,
};
use std::sync::Arc;

#[tokio::test]
async fn stateless_function_call_is_replayed_by_the_client_without_server_history() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let prompt = "What is the weather in Shanghai?";
    mock.when(RequestMatcher::PromptWithTools {
        prompt: mock_prompts::build_prompt(prompt),
        tool_names: vec!["get_weather".to_string()],
    })
    .respond_with(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
            "get_weather",
            r#"{"location":"Shanghai"}"#,
        )
        .with_thought_signature("gemini-thought-signature")]),
    )
    .await;
    mock.set_default_response(ResponseTemplate::new(
        "The temperature in Shanghai is 22°C.",
    ))
    .await;

    let tools = serde_json::json!([{
        "type": "function",
        "name": "get_weather",
        "description": "Get the current weather for a location.",
        "parameters": {
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"]
        }
    }]);

    // First turn: Cloud returns a requested function call, but does not run it.
    let first = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": prompt,
            "store": false,
            "stream": false,
            "tools": tools,
        }))
        .await;
    assert_eq!(
        first.status_code(),
        200,
        "first turn failed: {}",
        first.text()
    );
    let first = first.json::<serde_json::Value>();
    assert_eq!(first["status"], "incomplete");
    assert_eq!(
        first["incomplete_details"]["reason"],
        "function_call_required"
    );
    assert_eq!(
        mock.chat_completion_call_count(),
        1,
        "a client-managed function call must not trigger a follow-up completion"
    );
    let function_call = first["output"]
        .as_array()
        .expect("first response has output")
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("first response returns a function_call")
        .clone();
    let call_id = function_call["call_id"]
        .as_str()
        .expect("function call has call_id")
        .to_string();
    assert_eq!(
        function_call["thought_signature"],
        "gemini-thought-signature"
    );

    // The caller executes the function and sends both the returned call item
    // and its result in a new stateless request. No response ID is referenced.
    let second = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "store": false,
            "stream": false,
            "input": [
                {"role": "user", "content": prompt},
                {
                    "type": "message",
                    "id": "msg_first_turn",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "I will look that up.",
                        "annotations": [],
                        "logprobs": []
                    }]
                },
                function_call,
                {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": "{\"temperature_c\":22}"
                }
            ],
            "tools": tools,
        }))
        .await;
    assert_eq!(
        second.status_code(),
        200,
        "second turn failed: {}",
        second.text()
    );
    let second = second.json::<serde_json::Value>();
    assert_eq!(second["status"], "completed");

    // The provider receives the raw assistant output text and replayed custom
    // tool call in one assistant turn, followed by the client-produced tool
    // result. Cloud did not execute the custom function.
    let params = mock
        .last_chat_params()
        .await
        .expect("second request reached provider");
    let assistant = params
        .messages
        .iter()
        .find(|message| {
            message.role == MessageRole::Assistant && message.tool_calls.as_ref().is_some()
        })
        .expect("provider received replayed assistant tool call");
    assert_eq!(
        assistant.content.as_ref(),
        Some(&serde_json::json!("I will look that up.")),
        "assistant output text and its function call must remain one provider turn"
    );
    let tool_calls = assistant.tool_calls.as_ref().expect("tool calls present");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id.as_deref(), Some(call_id.as_str()));
    assert_eq!(tool_calls[0].function.name.as_deref(), Some("get_weather"));
    assert_eq!(
        tool_calls[0].thought_signature.as_deref(),
        Some("gemini-thought-signature")
    );

    let tool_result = params
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("provider received client-produced tool result");
    assert_eq!(tool_result.tool_call_id.as_deref(), Some(call_id.as_str()));
    assert_eq!(
        tool_result.content.as_ref(),
        Some(&serde_json::json!("{\"temperature_c\":22}"))
    );
    assert_eq!(
        mock.chat_completion_call_count(),
        2,
        "each client request should result in exactly one completion"
    );
}

#[tokio::test]
async fn stateless_response_without_tools_completes_after_one_completion() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    mock.set_default_response(ResponseTemplate::new("A normal answer."))
        .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": "Say hello.",
            "store": false,
            "stream": false,
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "response failed: {}",
        response.text()
    );
    let response = response.json::<serde_json::Value>();
    assert_eq!(response["status"], "completed");
    assert_eq!(response["tools"], serde_json::json!([]));
    assert_eq!(mock.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn stateless_custom_web_search_function_is_not_executed_as_a_builtin_tool() {
    // Install a working built-in web-search provider. If the custom function
    // below were accidentally claimed by the built-in executor, this response
    // would continue after server-side search rather than pause for the client.
    let (server, _database, mock) = setup_test_server_with_search_providers(
        Arc::new(MockWebSearchProvider::default_results()),
        None,
    )
    .await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let prompt = "Search the web for the current weather in Shanghai.";
    mock.when(RequestMatcher::PromptWithTools {
        prompt: mock_prompts::build_prompt(prompt),
        tool_names: vec!["web_search".to_string()],
    })
    .respond_with(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
            "web_search",
            r#"{"query":"Shanghai weather"}"#,
        )]),
    )
    .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": prompt,
            "store": false,
            "stream": false,
            "tools": [{
                "type": "function",
                "name": "web_search",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "response failed: {}",
        response.text()
    );
    let response = response.json::<serde_json::Value>();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"]["reason"],
        "function_call_required"
    );
    assert!(response["output"]
        .as_array()
        .expect("response has output")
        .iter()
        .any(|item| item["type"] == "function_call" && item["name"] == "web_search"));
}

#[tokio::test]
async fn stateless_mcp_tools_are_rejected_before_provider_work() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": "What is the weather?",
            "store": false,
            "stream": false,
            "tools": [
                {
                    "type": "function",
                    "name": "weather:get_weather",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "mcp",
                    "server_label": "weather",
                    "server_url": "https://example.com/mcp",
                    "require_approval": "never"
                }
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 400, "response: {}", response.text());
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(
        mock.last_chat_params().await.is_none(),
        "MCP should be rejected before provider work"
    );
}
