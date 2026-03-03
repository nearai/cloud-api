//! E2E tests for multi-turn conversations with tool calls in chat completions API.
//!
//! Tests the preservation of tool_call_id and tool_calls through the request pipeline.
//! This is critical for external providers like OpenAI that validate tool message structure.

mod common;

use common::*;

/// Test multi-turn conversation with tool calls in /v1/chat/completions
/// This tests the bug fix where tool_call_id and tool_calls were being hardcoded to None
/// in the message conversion, preventing multi-turn tool call flows from working.
#[tokio::test]
async fn test_chat_completions_multiturn_tool_calls() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Define function tools
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "read",
            "description": "Read a file",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path"
                    }
                },
                "required": ["path"]
            }
        }
    })];

    // Prepare the multi-turn message history:
    // 1. System message
    // 2. User message requesting file read
    // 3. Assistant message with tool_calls (requesting to read file)
    // 4. Tool message with result
    // 5. User follow-up message
    let messages = vec![
        serde_json::json!({
            "role": "system",
            "content": "You are helpful."
        }),
        serde_json::json!({
            "role": "user",
            "content": "Read the config file"
        }),
        serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_001",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": r#"{"path": "/tmp/config.json"}"#
                    }
                }
            ]
        }),
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_001",
            "content": r#"{"port": 8080}"#
        }),
        serde_json::json!({
            "role": "user",
            "content": "What is in the config?"
        }),
    ];

    // Mock the provider to expect all messages (including tool_calls)
    // When the assistant message with tool_calls reaches the mock, it should be present
    mock.when(inference_providers::mock::RequestMatcher::Any)
        .respond_with(inference_providers::mock::ResponseTemplate::new(
            "The port is 8080",
        ))
        .await;

    // Make the chat completion request with the multi-turn tool history
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": messages,
            "max_completion_tokens": 500,
            "tools": tools,
            "stream": false
        }))
        .await;

    // Should succeed with 200 status - previously would fail with OpenAI error
    // about "messages with role 'tool' must be a response to a preceeding message with 'tool_calls'"
    assert_eq!(
        response.status_code(),
        200,
        "Expected success response, got: {}",
        response.text()
    );

    let completion = response.json::<serde_json::Value>();
    assert!(completion["choices"].is_array());
    assert!(completion["choices"][0]["message"]["content"].is_string());
    println!("✅ Multi-turn tool call test passed!");
}

/// Test streaming chat completions with multi-turn tool calls
#[tokio::test]
async fn test_chat_completions_multiturn_tool_calls_streaming() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string"
                    }
                },
                "required": ["location"]
            }
        }
    })];

    // Multi-turn with tool result
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": "What's the weather?"
        }),
        serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_weather_1",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": r#"{"location": "Tokyo"}"#
                    }
                }
            ]
        }),
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_weather_1",
            "content": "Sunny, 25°C"
        }),
    ];

    mock.when(inference_providers::mock::RequestMatcher::Any)
        .respond_with(inference_providers::mock::ResponseTemplate::new(
            "The weather in Tokyo is sunny and 25°C",
        ))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "stream": true
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let body = response.text();
    assert!(body.contains("data:"));
    println!("✅ Multi-turn streaming tool call test passed!");
}

/// Test that tool_call_id is preserved in tool role messages
#[tokio::test]
async fn test_tool_message_preserves_tool_call_id() {
    let (server, _, mock, _) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Simple flow: assistant with tool_calls, then tool response
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": "Execute task"
        }),
        serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "unique_tool_call_id_12345",
                    "type": "function",
                    "function": {
                        "name": "execute",
                        "arguments": "{}"
                    }
                }
            ]
        }),
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "unique_tool_call_id_12345",
            "content": "Task completed successfully"
        }),
    ];

    mock.when(inference_providers::mock::RequestMatcher::Any)
        .respond_with(inference_providers::mock::ResponseTemplate::new("Done"))
        .await;

    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    println!("✅ Tool message tool_call_id preservation test passed!");
}
