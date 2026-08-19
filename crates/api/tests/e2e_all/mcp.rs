//! E2E coverage for retired client-mediated MCP flows.
//!
//! The stateless Responses API still permits server-side MCP work that can
//! finish in one request, but it cannot retain an approval request for a
//! client to resume later.

use crate::common::*;
use inference_providers::mock::{RequestMatcher, ResponseTemplate, ToolCall};
use services::responses::models::McpDiscoveredTool;
use services::responses::tools::{MockMcpClient, MockMcpClientFactory};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
async fn mcp_tools_without_approval_complete_in_one_stateless_request() {
    let list_tools_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let list_tools_calls_for_factory = list_tools_calls.clone();
    let tool_calls_for_factory = tool_calls.clone();

    let mut mock_factory = MockMcpClientFactory::new();
    mock_factory
        .expect_create_client()
        .withf(|url: &str, _| url == "https://example.com/mcp")
        .returning(move |_, _| {
            let list_tools_calls = list_tools_calls_for_factory.clone();
            let tool_calls = tool_calls_for_factory.clone();
            let mut client = MockMcpClient::new();

            client.expect_list_tools().returning(move || {
                list_tools_calls.fetch_add(1, Ordering::SeqCst);
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
            client
                .expect_call_tool()
                .withf(|name: &str, arguments| {
                    name == "get_weather" && arguments["location"] == "San Francisco"
                })
                .returning(move |_, _| {
                    tool_calls.fetch_add(1, Ordering::SeqCst);
                    Ok("Weather in San Francisco: Sunny, 72°F".to_string())
                });

            Ok(Box::new(client) as Box<dyn services::responses::tools::mcp::McpClient>)
        });

    let (server, _pool, mock) = setup_test_server_with_mcp_factory(Arc::new(mock_factory)).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let prompt = "What's the weather in San Francisco?";
    mock.when(RequestMatcher::PromptWithTools {
        prompt: mock_prompts::build_prompt(prompt),
        tool_names: vec!["weather:get_weather".to_string()],
    })
    .respond_with(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
            "weather:get_weather",
            serde_json::json!({"location": "San Francisco"}).to_string(),
        )]),
    )
    .await;
    mock.set_default_response(ResponseTemplate::new(
        "The weather in San Francisco is sunny and 72°F.",
    ))
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
                "type": "mcp",
                "server_label": "weather",
                "server_url": "https://example.com/mcp",
                "require_approval": "never"
            }]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "stateless MCP request failed: {}",
        response.text()
    );
    let response = response.json::<serde_json::Value>();
    assert_eq!(response["status"], "completed");
    assert_eq!(list_tools_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);

    let output = response["output"]
        .as_array()
        .expect("response output should be an array");
    let discovered = output
        .iter()
        .find(|item| item["type"] == "mcp_list_tools")
        .expect("MCP tools should be discovered during the request");
    assert_eq!(discovered["server_label"], "weather");
    assert_eq!(discovered["tools"][0]["name"], "get_weather");
    assert!(
        output
            .iter()
            .all(|item| item["type"] != "mcp_approval_request"),
        "require_approval=never must not produce a resumable approval request"
    );
}

#[tokio::test]
async fn mcp_tools_requiring_approval_are_rejected_by_stateless_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
            "input": "Check the weather",
            "store": false,
            "tools": [{
                "type": "mcp",
                "server_label": "weather",
                "server_url": "https://example.com/mcp",
                "require_approval": "always"
            }]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("require approval"));
}

#[tokio::test]
async fn mcp_approval_continuations_are_rejected_by_stateless_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "test-model",
            "store": false,
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": "mcpr_example",
                "approve": true
            }]
        }))
        .await;

    assert_eq!(response.status_code(), 400);
    let error = response.json::<api::models::ErrorResponse>();
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("MCP approval continuation"));
}
