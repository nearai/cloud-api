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
    assert_eq!(first["status"], "completed");
    assert!(first["incomplete_details"].is_null());
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
    assert_eq!(function_call["status"], "completed");
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
async fn stateless_stream_completes_parallel_function_items_and_replays_without_history() {
    let (server, _pool, mock, database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let prompt = "Compare the weather in Shanghai and Beijing.";
    let preamble = "I will check both cities.";
    let answer = "Shanghai is 22°C and Beijing is 18°C.";
    let arguments = [r#"{"location": "Shanghai"}"#, r#"{"location": "Beijing"}"#];
    mock.when(RequestMatcher::PromptWithTools {
        prompt: mock_prompts::build_prompt(prompt),
        tool_names: vec!["get_weather".to_string()],
    })
    .respond_with(ResponseTemplate::new(preamble).with_tool_calls(vec![
        ToolCall::new("get_weather", arguments[0]),
        ToolCall::new("get_weather", arguments[1]),
    ]))
    .await;
    mock.set_default_response(ResponseTemplate::new(answer))
        .await;

    let tools = serde_json::json!([{
        "type": "function",
        "name": "get_weather",
        "parameters": {
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"]
        }
    }]);
    let first = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "input": prompt,
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "tools": tools,
        }))
        .await;
    assert_eq!(first.status_code(), 200, "first stream: {}", first.text());
    let first_events = response_events(&first.text());
    assert!(first_events
        .iter()
        .all(|event| event["type"] != "response.failed"));
    let first_completed = first_events.last().expect("first stream has events");
    assert_eq!(first_completed["type"], "response.completed");
    let first_response = &first_completed["response"];
    assert_eq!(first_response["status"], "completed");
    assert!(first_response["incomplete_details"].is_null());
    assert_eq!(mock.chat_completion_call_count(), 1);

    let first_output = first_response["output"]
        .as_array()
        .expect("first stream has final output");
    assert_eq!(first_output.len(), 3, "text and two function calls");
    assert_eq!(first_output[0]["type"], "message");
    assert_eq!(first_output[0]["content"][0]["text"], preamble);

    // Every output item gets its own slot. In particular, a preceding text
    // item must not share output_index with the first function call.
    let added_items: Vec<_> = first_events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .collect();
    assert_eq!(added_items.len(), first_output.len());
    for (output_index, item) in first_output.iter().enumerate() {
        assert_eq!(item["status"], "completed");
        let added = added_items
            .iter()
            .find(|event| event["item"]["id"] == item["id"])
            .expect("every final output item was added");
        assert_eq!(added["output_index"], output_index);
    }

    let function_calls = &first_output[1..];
    assert_ne!(function_calls[0]["call_id"], function_calls[1]["call_id"]);
    for (index, call) in function_calls.iter().enumerate() {
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["arguments"], arguments[index]);
        assert_ne!(call["id"], call["call_id"]);

        let lifecycle: Vec<_> = first_events
            .iter()
            .filter(|event| event["item"]["id"] == call["id"] || event["item_id"] == call["id"])
            .collect();
        let types: Vec<_> = lifecycle
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        assert_eq!(types.first(), Some(&"response.output_item.added"));
        assert_eq!(types.last(), Some(&"response.output_item.done"));
        let arguments_done_index = types
            .iter()
            .position(|event_type| *event_type == "response.function_call_arguments.done")
            .expect("function arguments must finish before the item");
        assert!(
            arguments_done_index > 1,
            "arguments must have streamed deltas"
        );
        assert_eq!(arguments_done_index, lifecycle.len() - 2);
        assert!(types[1..arguments_done_index]
            .iter()
            .all(|event_type| *event_type == "response.function_call_arguments.delta"));
        let streamed_arguments: String = lifecycle[1..arguments_done_index]
            .iter()
            .map(|event| event["delta"].as_str().expect("argument delta is text"))
            .collect();
        assert_eq!(streamed_arguments, arguments[index]);
        assert_eq!(lifecycle[0]["item"]["status"], "in_progress");
        assert_eq!(lifecycle[0]["item"]["arguments"], "");
        assert_eq!(
            lifecycle[arguments_done_index]["arguments"],
            arguments[index]
        );
        assert_eq!(lifecycle[arguments_done_index]["item_id"], call["id"]);
        assert_ne!(lifecycle[arguments_done_index]["item_id"], call["call_id"]);
        assert_eq!(lifecycle.last().unwrap()["item"], *call);
        for event in lifecycle {
            assert_eq!(event["output_index"], index + 1);
        }
    }

    // Replay exactly the items a streaming client received, including the
    // preamble and both parallel calls, with client-produced results.
    let results = [r#"{"temperature_c":22}"#, r#"{"temperature_c":18}"#];
    let mut input = vec![serde_json::json!({"role": "user", "content": prompt})];
    input.extend(first_output.iter().cloned());
    for (call, result) in function_calls.iter().zip(results) {
        input.push(serde_json::json!({
            "type": "function_call_output",
            "call_id": call["call_id"],
            "output": result,
        }));
    }
    let second = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model,
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "input": input,
            "tools": tools,
        }))
        .await;
    assert_eq!(
        second.status_code(),
        200,
        "replay stream: {}",
        second.text()
    );
    let second_events = response_events(&second.text());
    assert!(second_events
        .iter()
        .all(|event| event["type"] != "response.failed"));
    let second_completed = second_events.last().expect("replay stream has events");
    assert_eq!(second_completed["type"], "response.completed");
    let second_response = &second_completed["response"];
    assert_eq!(second_response["status"], "completed");
    assert_eq!(second_response["output"].as_array().unwrap().len(), 1);
    assert_eq!(second_response["output"][0]["content"][0]["text"], answer);
    assert_eq!(mock.chat_completion_call_count(), 2);

    let params = mock
        .last_chat_params()
        .await
        .expect("replay reached provider");
    let assistant = params
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Assistant && message.tool_calls.is_some())
        .expect("provider received the replayed assistant turn");
    assert_eq!(assistant.content, Some(serde_json::json!(preamble)));
    let replayed_calls = assistant.tool_calls.as_ref().unwrap();
    assert_eq!(replayed_calls.len(), 2);
    let replayed_results: Vec<_> = params
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect();
    assert_eq!(replayed_results.len(), 2);
    for (index, call) in function_calls.iter().enumerate() {
        let call_id = call["call_id"].as_str().unwrap();
        assert_eq!(replayed_calls[index].id.as_deref(), Some(call_id));
        assert_eq!(
            replayed_calls[index].function.arguments.as_deref(),
            Some(arguments[index])
        );
        assert_eq!(
            replayed_results[index].tool_call_id.as_deref(),
            Some(call_id)
        );
        assert_eq!(
            replayed_results[index].content,
            Some(serde_json::json!(results[index]))
        );
    }

    // Query only this test's generated IDs, never shared database totals.
    let client = database.pool().get().await.unwrap();
    for response in [first_response, second_response] {
        let id = response["id"].as_str().unwrap();
        let id = uuid::Uuid::parse_str(id.strip_prefix("resp_").unwrap()).unwrap();
        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM responses WHERE id = $1), \
                 EXISTS(SELECT 1 FROM response_items WHERE response_id = $1)",
                &[&id],
            )
            .await
            .unwrap();
        assert!(
            !row.get::<_, bool>(0),
            "response history must not be stored"
        );
        assert!(!row.get::<_, bool>(1), "response items must not be stored");
    }
}

fn response_events(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("SSE data must be valid JSON"))
        .collect()
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
    // would continue after server-side search rather than return the call to the client.
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
    assert_eq!(response["status"], "completed");
    assert!(response["incomplete_details"].is_null());
    assert!(response["output"]
        .as_array()
        .expect("response has output")
        .iter()
        .any(|item| item["type"] == "function_call"
            && item["name"] == "web_search"
            && item["status"] == "completed"));
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
