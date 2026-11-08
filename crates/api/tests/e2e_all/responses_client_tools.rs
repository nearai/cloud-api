//! HTTP coverage for client-managed Lite, namespace, and freeform tools.

use super::responses_hosted_search::{
    completed_response as completed_http_response, RecordingSearchProvider,
};
use crate::common::*;
use inference_providers::{
    mock::{ResponseTemplate, ToolCall},
    MessageRole,
};
use serde_json::{json, Value};
use services::responses::client_tools::normalize_request;
use std::sync::Arc;

fn lite_request(model: &str) -> Value {
    let mut request: Value =
        serde_json::from_str(include_str!("fixtures/responses_lite.json")).unwrap();
    request["model"] = json!(model);
    request
}

// The mock provider speaks the existing flat Chat Completions tool protocol.
// Derive its aliases from normalization, but assert the public wire contract
// independently below, including exact external names, namespaces, and bytes.
fn provider_tool_names(request: &Value) -> Vec<String> {
    let (normalized, _) = normalize_request(request.clone()).unwrap();
    normalized["tools"]
        .as_array()
        .expect("normalized tools")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

fn response_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("valid SSE JSON"))
        .collect()
}

fn completed_response(events: &[Value]) -> &Value {
    assert!(events
        .iter()
        .all(|event| event["type"] != "response.failed"));
    let completed = events.last().expect("response has events");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    &completed["response"]
}

#[tokio::test]
async fn lite_custom_call_streams_and_replays_as_a_client_tool() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let mut request = lite_request(&model);
    let names = provider_tool_names(&request);
    let script = "const marker = 'NEAR_TOOL_OK';\ntext(marker);\n";
    let wrapper = json!({"input": script}).to_string();
    mock.set_default_response(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(&names[0], &wrapper)]),
    )
    .await;

    let first = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;
    assert_eq!(first.status_code(), 200, "{}", first.text());
    let events = response_events(&first.text());
    let response = completed_response(&events);
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 1);
    let call = &output[0];
    assert_eq!(call["type"], "custom_tool_call");
    assert_eq!(call["namespace"], "functions");
    assert_eq!(call["name"], "exec");
    assert_eq!(call["input"], script);
    assert_eq!(call["status"], "completed");
    assert!(call.get("arguments").is_none());
    assert_ne!(call["id"], call["call_id"]);

    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .unwrap();
    assert_eq!(added["item"]["type"], "custom_tool_call");
    assert_eq!(added["item"]["input"], "");
    assert_eq!(added["item"]["status"], "in_progress");
    let input_done = events
        .iter()
        .position(|event| event["type"] == "response.custom_tool_call_input.done")
        .expect("custom input completion is emitted");
    assert_eq!(events[input_done]["input"], script);
    assert_eq!(events[input_done]["item_id"], call["id"]);
    assert_eq!(events[input_done]["output_index"], 0);
    assert_eq!(events[input_done + 1]["type"], "response.output_item.done");
    assert_eq!(events[input_done + 1]["item"], *call);
    assert!(events.iter().all(|event| !event["type"]
        .as_str()
        .unwrap()
        .starts_with("response.function_call_arguments.")));
    assert_eq!(mock.chat_completion_call_count(), 1);

    let params = mock.last_chat_params().await.unwrap();
    assert_eq!(params.messages[0].role, MessageRole::System);
    assert_eq!(
        params
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .count(),
        1,
        "instructions and developer input must form one leading system message"
    );
    let system = params.messages[0]
        .content
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(system.contains("Use only the synthetic tools provided for this test."));
    assert!(system.contains("Print the synthetic marker with the client exec tool."));

    let input = request["input"].as_array_mut().unwrap();
    input.push(call.clone());
    input.push(json!({
        "type": "custom_tool_call_output",
        "call_id": call["call_id"],
        "output": "NEAR_TOOL_OK"
    }));
    mock.set_default_response(ResponseTemplate::new("The client returned NEAR_TOOL_OK."))
        .await;
    let second = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;
    assert_eq!(second.status_code(), 200, "{}", second.text());
    let events = response_events(&second.text());
    let response = completed_response(&events);
    assert_eq!(
        response["output"][0]["content"][0]["text"],
        "The client returned NEAR_TOOL_OK."
    );
    assert_eq!(mock.chat_completion_call_count(), 2);
    let params = mock.last_chat_params().await.unwrap();
    let assistant = params
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Assistant && message.tool_calls.is_some())
        .expect("custom call replay reached the provider");
    let replayed = &assistant.tool_calls.as_ref().unwrap()[0];
    assert_eq!(replayed.id.as_deref(), call["call_id"].as_str());
    assert_eq!(replayed.function.name.as_deref(), Some(names[0].as_str()));
    let replayed_arguments: Value =
        serde_json::from_str(replayed.function.arguments.as_deref().unwrap()).unwrap();
    assert_eq!(replayed_arguments, json!({"input": script}));
    let tool_result = params
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(
        tool_result.tool_call_id.as_deref(),
        call["call_id"].as_str()
    );
    assert_eq!(tool_result.content, Some(json!("NEAR_TOOL_OK")));
}

#[tokio::test]
async fn namespaced_functions_keep_distinct_names_and_raw_arguments() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let tools: Vec<_> = ["catalog", "archive"]
        .into_iter()
        .map(|namespace| {
            json!({
                "type": "namespace", "name": namespace, "description": "Synthetic lookup tools.",
                "tools": [{"type": "function", "name": "lookup", "parameters": {
                    "type": "object", "properties": {"key": {"type": "string"}},
                    "required": ["key"], "additionalProperties": false
                }}]
            })
        })
        .collect();
    let request = json!({
        "model": model, "store": false, "stream": true,
        "input": "Compare the two synthetic lookups.", "parallel_tool_calls": true, "tools": tools
    });
    let names = provider_tool_names(&request);
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    let arguments = ["{\n  \"key\" : \"a\"\n}", "{ \"key\": \"b\" }"];
    mock.set_default_response(ResponseTemplate::new("").with_tool_calls(vec![
        ToolCall::new(&names[0], arguments[0]),
        ToolCall::new(&names[1], arguments[1]),
    ]))
    .await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let events = response_events(&response.text());
    let output = completed_response(&events)["output"].as_array().unwrap();
    assert_eq!(output.len(), 2);
    for (index, namespace) in ["catalog", "archive"].into_iter().enumerate() {
        assert_eq!(output[index]["type"], "function_call");
        assert_eq!(output[index]["namespace"], namespace);
        assert_eq!(output[index]["name"], "lookup");
        assert_eq!(output[index]["arguments"], arguments[index]);
        assert_eq!(output[index]["status"], "completed");
    }
    assert_ne!(output[0]["id"], output[1]["id"]);
    assert_ne!(output[0]["call_id"], output[1]["call_id"]);
    assert_eq!(mock.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn tool_choice_follows_current_tools_during_replay() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_default_response(ResponseTemplate::new("Checkpoint saved."))
        .await;

    let checkpoint = json!({
        "model": model, "store": false, "stream": true,
        "parallel_tool_calls": false,
        "input": [
            {"role": "user", "content": "Read the fixture."},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "Fixture read."},
            {"role": "user", "content": "Summarize progress."}
        ]
    });
    let mut required = checkpoint.clone();
    required["tools"] = json!([]);
    required["tool_choice"] = json!("required");
    let rejected = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&required)
        .await;
    assert_eq!(rejected.status_code(), 400, "{}", rejected.text());
    let error = &rejected.json::<Value>()["error"];
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(
        error["message"],
        "tool_choice 'required' requires at least one declared tool"
    );
    assert_eq!(mock.chat_completion_call_count(), 0);

    let lookup = json!([{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]);
    for (index, (tools, choice, expected_choice)) in [
        (None, "auto", None),
        (Some(json!([])), "auto", None),
        (Some(lookup.clone()), "auto", Some("auto")),
        (Some(json!([])), "none", None),
        (Some(lookup), "none", Some("none")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = checkpoint.clone();
        request["tool_choice"] = json!(choice);
        if let Some(tools) = tools {
            request["tools"] = tools;
        }
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;
        let response = completed_http_response(&result, true);
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "Checkpoint saved."
        );

        assert_eq!(mock.chat_completion_call_count(), index + 1);
        let upstream = serde_json::to_value(mock.last_chat_params().await.unwrap()).unwrap();
        assert!(upstream["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"
                && message["tool_call_id"] == "call_1"
                && message["content"] == "Fixture read."));
        assert_eq!(
            upstream.get("tool_choice"),
            expected_choice.map(|choice| json!(choice)).as_ref()
        );
        if expected_choice == Some("auto") {
            assert_eq!(upstream["tools"][0]["function"]["name"], "lookup");
        } else {
            assert!(upstream.get("tools").is_none());
        }
    }
}

#[tokio::test]
async fn standard_named_function_choice_reaches_the_provider() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let request = json!({
        "model": model, "store": false, "stream": false,
        "input": "Read the synthetic value.",
        "tools": [{"type": "function", "name": "lookup", "parameters": {
            "type": "object", "properties": {}, "additionalProperties": false
        }}],
        "tool_choice": {"type": "function", "name": "lookup"}
    });
    let names = provider_tool_names(&request);
    mock.set_default_response(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(&names[0], "{}")]),
    )
    .await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let response = response.json::<Value>();
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["type"], "function_call");
    assert_eq!(response["output"][0]["name"], "lookup");
    let params = mock.last_chat_params().await.unwrap();
    assert_eq!(
        serde_json::to_value(params.tool_choice).unwrap(),
        json!({"type": "function", "function": {"name": names[0]}})
    );
    assert_eq!(mock.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn malformed_custom_input_fails_without_executing_or_completing_the_call() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let request = lite_request(&model);
    let names = provider_tool_names(&request);
    mock.set_default_response(
        ResponseTemplate::new("")
            .with_tool_calls(vec![ToolCall::new(&names[0], r#"{"input":42}"#)]),
    )
    .await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let events = response_events(&response.text());
    assert!(events.iter().all(|event| {
        event["type"] != "response.completed" && event["type"] != "response.output_item.done"
    }));
    let failed = events.last().expect("failed stream has a terminal event");
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["status"], "failed");
    assert!(!failed["response"]["error"]["message"]
        .as_str()
        .expect("failure includes a useful error")
        .is_empty());
    assert_eq!(mock.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn undeclared_function_calls_complete_and_replay_client_errors() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let arguments = " \n{\"plan\" : [{\"step\":\"Audit\",\"status\":\"in_progress\"}]}\n";
    let client_error = "unsupported call: update_plan";

    for (index, stream) in [false, true].into_iter().enumerate() {
        let mut request = json!({
            "model": model, "store": false, "stream": stream,
            "input": [{"role":"user","content":"Audit the synthetic fixture."}],
            "tools": [{"type":"function","name":"exec_command","parameters":{
                "type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]
            }}]
        });
        mock.set_default_response(
            ResponseTemplate::new("")
                .with_tool_calls(vec![ToolCall::new("update_plan", arguments)]),
        )
        .await;
        let first = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;
        let response = completed_http_response(&first, stream);
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        let call = &output[0];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["name"], "update_plan");
        assert_eq!(call["arguments"], arguments);
        assert_eq!(call["call_id"], "call_0");
        assert_ne!(call["id"], call["call_id"]);
        assert_eq!(call["status"], "completed");
        assert!(call.get("namespace").is_none());
        if stream {
            let events = response_events(&first.text());
            let done = events
                .iter()
                .find(|event| event["type"] == "response.function_call_arguments.done")
                .unwrap();
            assert_eq!(done["item_id"], call["id"]);
            assert_eq!(done["arguments"], arguments);
            let item_done = events
                .iter()
                .find(|event| event["type"] == "response.output_item.done")
                .unwrap();
            assert_eq!(item_done["item"], *call);
        }

        let input = request["input"].as_array_mut().unwrap();
        input.push(call.clone());
        input.push(json!({
            "type":"function_call_output","call_id":call["call_id"],"output":client_error
        }));
        mock.set_default_response(ResponseTemplate::new(
            "Continuing without the unavailable tool.",
        ))
        .await;
        let second = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;
        let response = completed_http_response(&second, stream);
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "Continuing without the unavailable tool."
        );
        let params = mock.last_chat_params().await.unwrap();
        let tools = params.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "exec_command");
        let replayed = params
            .messages
            .iter()
            .find_map(|message| message.tool_calls.as_ref())
            .unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id.as_deref(), Some("call_0"));
        assert_eq!(replayed[0].function.name.as_deref(), Some("update_plan"));
        assert_eq!(replayed[0].function.arguments.as_deref(), Some(arguments));
        let result = params
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .unwrap();
        assert_eq!(result.tool_call_id.as_deref(), Some("call_0"));
        assert_eq!(result.content, Some(json!(client_error)));
        assert_eq!(mock.chat_completion_call_count(), 2 * (index + 1));
    }
}

#[tokio::test]
async fn removed_client_tool_aliases_complete_and_replay_client_errors() {
    let (server, _pool, mock, _database) = setup_test_server_with_pool().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let client_error = "unsupported call: removed tool";
    let histories = [
        (
            json!({"type":"custom_tool_call","call_id":"call_old","namespace":"functions","name":"exec","input":"text('old');"}),
            json!({"type":"custom_tool_call_output","call_id":"call_old","output":"old result"}),
            json!({"input":"text('old');"}).to_string(),
        ),
        (
            json!({"type":"function_call","call_id":"call_old","namespace":"archive","name":"lookup","arguments":" {\"key\":\"old\"} "}),
            json!({"type":"function_call_output","call_id":"call_old","output":"old result"}),
            " {\"key\":\"old\"} ".to_string(),
        ),
    ];

    for (history, old_result, old_arguments) in histories {
        // A valid custom wrapper and a missing input must both remain opaque
        // once the old tool has been removed from the current catalog.
        for arguments in [" {\"input\":\"text('new');\"} ", "{}"] {
            for stream in [false, true] {
                let mut request = json!({
                    "model":model,"store":false,"stream":stream,
                    "input":[
                        {"role":"user","content":"Inspect the old synthetic value."},
                        history,old_result,
                        {"role":"user","content":"Continue with the available tools."}
                    ],
                    "tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}}]
                });
                let (normalized, _) = normalize_request(request.clone()).unwrap();
                let alias = normalized["input"][1]["name"].as_str().unwrap();
                assert_ne!(alias, history["name"].as_str().unwrap());
                assert_eq!(normalized["tools"].as_array().unwrap().len(), 1);
                assert_eq!(normalized["tools"][0]["name"], "exec_command");
                mock.set_default_response(
                    ResponseTemplate::new("")
                        .with_tool_calls(vec![ToolCall::new(alias, arguments)]),
                )
                .await;
                let first = server
                    .post("/v1/responses")
                    .add_header("Authorization", format!("Bearer {api_key}"))
                    .json(&request)
                    .await;
                let response = completed_http_response(&first, stream);
                let output = response["output"].as_array().unwrap();
                assert_eq!(output.len(), 1);
                let call = &output[0];
                assert_eq!(call["type"], "function_call");
                assert_eq!(call["name"], alias);
                assert_eq!(call["arguments"], arguments);
                assert_eq!(call["call_id"], "call_0");
                assert!(call.get("namespace").is_none());
                assert!(call.get("input").is_none());
                if stream {
                    let events = response_events(&first.text());
                    assert!(events.iter().all(|event| !event["type"]
                        .as_str()
                        .unwrap()
                        .starts_with("response.custom_tool_call_input.")));
                    let done = events
                        .iter()
                        .find(|event| event["type"] == "response.function_call_arguments.done")
                        .unwrap();
                    assert_eq!(done["arguments"], arguments);
                    assert_eq!(done["item_id"], call["id"]);
                }

                let input = request["input"].as_array_mut().unwrap();
                input.push(call.clone());
                input.push(json!({
                    "type":"function_call_output","call_id":call["call_id"],"output":client_error
                }));
                mock.set_default_response(ResponseTemplate::new(
                    "Recovered from the removed tool.",
                ))
                .await;
                let second = server
                    .post("/v1/responses")
                    .add_header("Authorization", format!("Bearer {api_key}"))
                    .json(&request)
                    .await;
                let response = completed_http_response(&second, stream);
                assert_eq!(
                    response["output"][0]["content"][0]["text"],
                    "Recovered from the removed tool."
                );
                let params = mock.last_chat_params().await.unwrap();
                let tools = params.tools.as_ref().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].function.name, "exec_command");
                let calls: Vec<_> = params
                    .messages
                    .iter()
                    .filter_map(|message| message.tool_calls.as_ref())
                    .flatten()
                    .collect();
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id.as_deref(), Some("call_old"));
                assert_eq!(calls[0].function.name.as_deref(), Some(alias));
                assert_eq!(
                    calls[0].function.arguments.as_deref(),
                    Some(old_arguments.as_str())
                );
                assert_eq!(calls[1].id.as_deref(), Some("call_0"));
                assert_eq!(calls[1].function.name.as_deref(), Some(alias));
                assert_eq!(calls[1].function.arguments.as_deref(), Some(arguments));
                let result = params
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("call_0"))
                    .unwrap();
                assert_eq!(result.content, Some(json!(client_error)));
            }
        }
    }
    assert_eq!(mock.chat_completion_call_count(), 16);
}

#[tokio::test]
async fn undeclared_search_names_return_client_calls_without_hosted_execution() {
    let search = Arc::new(RecordingSearchProvider::default());
    let (server, _database, mock) =
        setup_test_server_with_search_providers(search.clone(), Some(search.clone())).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let arguments = r#"{"query":"synthetic references"}"#;

    for name in ["web_search", "web_context_search"] {
        for stream in [false, true] {
            mock.set_default_response(
                ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(name, arguments)]),
            )
            .await;
            let result = server
                .post("/v1/responses")
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model":model,"store":false,"stream":stream,"input":"Audit the fixture.",
                    "tools":[{"type":"function","name":"exec_command"}]
                }))
                .await;
            let response = completed_http_response(&result, stream);
            let output = response["output"].as_array().unwrap();
            assert_eq!(output.len(), 1);
            assert_eq!(output[0]["type"], "function_call");
            assert_eq!(output[0]["name"], name);
            assert_eq!(output[0]["arguments"], arguments);
            assert_eq!(output[0]["call_id"], "call_0");
            if stream {
                assert!(response_events(&result.text())
                    .iter()
                    .all(|event| !event["type"]
                        .as_str()
                        .unwrap()
                        .starts_with("response.web_search_call.")));
            }
            assert!(search.web_params.lock().unwrap().is_empty());
            assert!(search.context_params.lock().unwrap().is_empty());
        }
    }
    assert_eq!(mock.chat_completion_call_count(), 4);
}
