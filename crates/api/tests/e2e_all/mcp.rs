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

/// A foreign or unknown MCP approval_request_id must be rejected BEFORE the
/// response row is created (issue nearai/infra#190): no response.created event
/// is emitted and nothing is persisted, so no orphaned in-progress response is
/// left behind, and unknown vs foreign IDs are indistinguishable.
#[tokio::test]
async fn test_mcp_foreign_approval_request_rejected_before_response_creation() {
    // Mock MCP server with one tool that always requires approval.
    let mut mock_factory = MockMcpClientFactory::new();
    mock_factory
        .expect_create_client()
        .withf(|url: &str, _| url == "https://example.com/mcp")
        .returning(move |_, _| {
            let mut client = MockMcpClient::new();
            client.expect_list_tools().returning(move || {
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
                .returning(|_, _| Ok("Weather: Sunny".to_string()));
            Ok(Box::new(client) as Box<dyn services::responses::tools::mcp::McpClient>)
        });

    let mcp_factory = Arc::new(mock_factory);
    let (server, _pool, mock) = setup_test_server_with_mcp_factory(mcp_factory).await;
    setup_qwen_model(&server).await;

    let org_a = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let key_a = get_api_key_for_org(&server, org_a.id).await;
    let org_b = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let key_b = get_api_key_for_org(&server, org_b.id).await;

    let mcp_tool = serde_json::json!({
        "type": "mcp",
        "server_label": "weather_server",
        "server_url": "https://example.com/mcp",
        "require_approval": "always"
    });

    // Org A: trigger a tool call so a real approval request gets stored.
    use crate::common::mock_prompts;
    let prompt = mock_prompts::build_prompt("What's the weather in San Francisco?");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        prompt,
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

    let resp_a = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_a}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "What's the weather in San Francisco?",
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;
    assert_eq!(
        resp_a.status_code(),
        200,
        "org A turn failed: {}",
        resp_a.text()
    );
    let resp_a_obj = resp_a.json::<api::models::ResponseObject>();
    assert_eq!(resp_a_obj.status, api::models::ResponseStatus::Incomplete);

    let approval_request_id = resp_a_obj
        .output
        .iter()
        .find_map(|item| {
            if let api::models::ResponseOutputItem::McpApprovalRequest { id, .. } = item {
                Some(id.clone())
            } else {
                None
            }
        })
        .expect("org A should receive an mcp_approval_request");

    // Org B: non-streaming attempt referencing org A's approval request.
    let foreign_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": approval_request_id,
                "approve": true
            }],
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;
    assert_eq!(
        foreign_attempt.status_code(),
        404,
        "foreign approval_request_id must be a non-enumerating 404: {}",
        foreign_attempt.text()
    );

    // Org B: unknown approval id gives the same status (non-enumerating).
    let unknown_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": format!("mcpr_{}", uuid::Uuid::new_v4().simple()),
                "approve": true
            }],
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;
    assert_eq!(unknown_attempt.status_code(), 404);

    // Org B: streaming attempt proves the rejection happens BEFORE the
    // response row is created: the stream must contain response.failed but
    // never response.created (which is emitted right after persistence).
    let streaming_attempt = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_b}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": approval_request_id,
                "approve": true
            }],
            "stream": true,
            "tools": [mcp_tool.clone()]
        }))
        .await;
    assert_eq!(streaming_attempt.status_code(), 200, "SSE transport is 200");
    let sse_body = streaming_attempt.text();
    assert!(
        !sse_body.contains("response.created"),
        "no response may be created for a foreign approval_request_id"
    );
    assert!(
        sse_body.contains("response.failed"),
        "stream must emit response.failed for a foreign approval_request_id"
    );
    assert!(
        sse_body.contains("\"status_code\":404"),
        "failure must carry the non-enumerating 404 status"
    );

    // Org A can still resolve its own approval request afterwards.
    let approve_prompt =
        mock_prompts::build_prompt("What's the weather in San Francisco? Weather: Sunny");
    mock.when(inference_providers::mock::RequestMatcher::ExactPrompt(
        approve_prompt,
    ))
    .respond_with(inference_providers::mock::ResponseTemplate::new(
        "It is sunny in San Francisco.",
    ))
    .await;

    let own_approval = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key_a}"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "previous_response_id": resp_a_obj.id,
            "input": [{
                "type": "mcp_approval_response",
                "approval_request_id": approval_request_id,
                "approve": true
            }],
            "stream": false,
            "tools": [mcp_tool.clone()]
        }))
        .await;
    assert_eq!(
        own_approval.status_code(),
        200,
        "owner approval must still work: {}",
        own_approval.text()
    );
}
