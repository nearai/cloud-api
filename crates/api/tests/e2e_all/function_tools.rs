//! E2E coverage for client-managed function tools on stateless Responses.

use crate::common::*;
use inference_providers::{
    mock::{RequestMatcher, ResponseTemplate, ToolCall},
    MessageRole,
};
use services::responses::models::McpDiscoveredTool;
use services::responses::tools::{MockMcpClient, MockMcpClientFactory};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

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
        )]),
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

    // The provider receives the raw assistant output text, then a standard
    // assistant-tool-call and the client-produced tool result; Cloud did not
    // execute the custom function.
    let params = mock
        .last_chat_params()
        .await
        .expect("second request reached provider");
    assert!(params.messages.iter().any(|message| {
        message.role == MessageRole::Assistant
            && message.tool_calls.is_none()
            && message.content.as_ref() == Some(&serde_json::json!("I will look that up."))
    }));
    let assistant = params
        .messages
        .iter()
        .find(|message| {
            message.role == MessageRole::Assistant && message.tool_calls.as_ref().is_some()
        })
        .expect("provider received replayed assistant tool call");
    let tool_calls = assistant.tool_calls.as_ref().expect("tool calls present");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id.as_deref(), Some(call_id.as_str()));
    assert_eq!(tool_calls[0].function.name.as_deref(), Some("get_weather"));

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
}

#[tokio::test]
async fn stateless_function_replay_rejects_input_between_call_and_output() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_example",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Shanghai\"}"
                },
                {"role": "user", "content": "This cannot interrupt the tool result."},
                {
                    "type": "function_call_output",
                    "call_id": "call_example",
                    "output": "{\"temperature\":22}"
                }
            ]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("before any message"));
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
async fn stateless_custom_function_colliding_with_discovered_mcp_tool_is_rejected_before_execution()
{
    let list_tools_calls = Arc::new(AtomicUsize::new(0));
    let mcp_tool_calls = Arc::new(AtomicUsize::new(0));
    let list_tools_calls_for_factory = list_tools_calls.clone();
    let mcp_tool_calls_for_factory = mcp_tool_calls.clone();

    let mut mock_factory = MockMcpClientFactory::new();
    mock_factory
        .expect_create_client()
        .withf(|url: &str, _| url == "https://example.com/mcp")
        .returning(move |_, _| {
            let list_tools_calls = list_tools_calls_for_factory.clone();
            let mcp_tool_calls = mcp_tool_calls_for_factory.clone();
            let mut client = MockMcpClient::new();

            client.expect_list_tools().returning(move || {
                list_tools_calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![McpDiscoveredTool {
                    name: "get_weather".to_string(),
                    description: Some("Get weather for a location".to_string()),
                    input_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"location": {"type": "string"}}
                    })),
                    annotations: None,
                }])
            });
            // The collision must be rejected before the model can cause an
            // MCP call. Allow a call only so the counter produces a clear
            // regression assertion rather than a mock expectation panic.
            client.expect_call_tool().times(0..).returning(move |_, _| {
                mcp_tool_calls.fetch_add(1, Ordering::SeqCst);
                Ok("unexpected MCP execution".to_string())
            });

            Ok(Box::new(client) as Box<dyn services::responses::tools::mcp::McpClient>)
        });

    let (server, _pool, mock) = setup_test_server_with_mcp_factory(Arc::new(mock_factory)).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
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
    assert!(error
        .error
        .message
        .contains("conflicts with a configured or discovered server-executed tool"));
    assert_eq!(list_tools_calls.load(Ordering::SeqCst), 1);
    assert_eq!(mcp_tool_calls.load(Ordering::SeqCst), 0);
    assert!(
        mock.last_chat_params().await.is_none(),
        "the request must be rejected before inference"
    );
}
