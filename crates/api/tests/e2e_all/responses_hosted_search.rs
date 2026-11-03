//! Request-scoped hosted-search regression coverage with in-process providers.

use crate::common::*;
use async_trait::async_trait;
use inference_providers::{
    mock::{MockProvider, RequestMatcher, ResponseTemplate, ToolCall},
    MessageRole,
};
use serde_json::{json, Value};
use services::responses::tools::{
    WebContextSearchParams, WebContextSearchProviderTrait, WebSearchError, WebSearchParams,
    WebSearchProviderTrait, WebSearchResult,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct RecordingSearchProvider {
    pub web_params: Mutex<Vec<WebSearchParams>>,
    pub context_params: Mutex<Vec<WebContextSearchParams>>,
    pub fail: bool,
    model_provider: Mutex<Option<Arc<MockProvider>>>,
    observed_tool_choices: Mutex<Vec<Value>>,
}

impl RecordingSearchProvider {
    fn results(&self, query: &str) -> Result<Vec<WebSearchResult>, WebSearchError> {
        if self.fail {
            return Err(WebSearchError::WebSearchRequestFailed(
                "synthetic search failure".to_string(),
            ));
        }
        Ok(vec![WebSearchResult {
            title: format!("Source for {query}"),
            url: format!("https://example.com/{}", query.replace(' ', "-")),
            snippet: format!("Grounded context about {query}."),
        }])
    }
}

#[async_trait]
impl WebSearchProviderTrait for RecordingSearchProvider {
    async fn search(
        &self,
        params: WebSearchParams,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let model_provider = self.model_provider.lock().unwrap().clone();
        if let Some(model_provider) = model_provider {
            let params = model_provider.last_chat_params().await.unwrap();
            self.observed_tool_choices
                .lock()
                .unwrap()
                .push(serde_json::to_value(params.tool_choice).unwrap());
        }
        let result = self.results(&params.query);
        self.web_params.lock().unwrap().push(params);
        result
    }
}

#[async_trait]
impl WebContextSearchProviderTrait for RecordingSearchProvider {
    async fn search_context(
        &self,
        params: WebContextSearchParams,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let result = self.results(&params.query);
        self.context_params.lock().unwrap().push(params);
        result
    }
}

pub(super) fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("Responses SSE JSON"))
        .collect()
}

pub(super) fn completed_response(result: &axum_test::TestResponse, stream: bool) -> Value {
    assert_eq!(result.status_code(), 200, "{}", result.text());
    assert_eq!(result.headers()["cache-control"], "no-store");
    let response = if stream {
        let events = events(&result.text());
        assert_eq!(events.first().unwrap()["type"], "response.created");
        assert_eq!(events.last().unwrap()["type"], "response.completed");
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], sequence);
            assert_ne!(event["type"], "response.failed");
        }
        events.last().unwrap()["response"].clone()
    } else {
        result.json::<Value>()
    };
    assert_eq!(response["status"], "completed");
    response
}

pub(super) fn output_text(response: &Value) -> &Value {
    response["output"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["type"] == "message")
        .and_then(|message| message["content"].as_array())
        .and_then(|parts| parts.iter().find(|part| part["type"] == "output_text"))
        .expect("final model answer")
}

pub(super) fn assert_citation(annotation: &Value, text: &str, phrase: &str, url: &str) {
    assert_eq!(annotation["type"], "url_citation");
    assert_eq!(annotation["url"], url);
    assert!(!annotation["title"].as_str().unwrap().is_empty());
    let start = annotation["start_index"].as_u64().unwrap() as usize;
    let end = annotation["end_index"].as_u64().unwrap() as usize;
    assert!(start < end && end <= text.chars().count());
    assert_eq!(
        text.chars()
            .skip(start)
            .take(end - start)
            .collect::<String>(),
        phrase
    );
}

pub(super) fn assert_search_lifecycle(events: &[Value], response: &Value) {
    for (output_index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        if item["type"] != "web_search_call" {
            continue;
        }
        let id = &item["id"];
        let related: Vec<&Value> = events
            .iter()
            .filter(|event| event["item_id"] == *id || event["item"]["id"] == *id)
            .collect();
        let kinds: Vec<&str> = related
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "response.output_item.added",
                "response.web_search_call.in_progress",
                "response.web_search_call.searching",
                "response.web_search_call.completed",
                "response.output_item.done",
            ]
        );
        for event in &related {
            assert_eq!(event["output_index"], output_index);
        }
        assert_eq!(related[0]["item"]["status"], "in_progress");
        assert_eq!(related.last().unwrap()["item"], *item);
        assert_eq!(item["status"], "completed");
    }
}

pub(super) async fn assert_no_history(database: &database::Database, response: &Value) {
    let id = response["id"].as_str().expect("response ID");
    let id = uuid::Uuid::parse_str(id.strip_prefix("resp_").unwrap()).unwrap();
    let client = database.pool().get().await.unwrap();
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
    assert!(
        !row.get::<_, bool>(1),
        "search and message items must not be stored"
    );
}

#[tokio::test]
async fn request_web_search_count_overrides_the_model_and_zero_skips_the_provider() {
    for stream in [false, true] {
        let provider = Arc::new(RecordingSearchProvider::default());
        let (server, database, mock) =
            setup_test_server_with_search_providers(provider.clone(), None).await;
        let model = setup_qwen_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        mock.when(RequestMatcher::ToolResultCount(0))
            .respond_with(
                ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
                    "web_search",
                    json!({"query":"count-test", "count": 9}).to_string(),
                )]),
            )
            .await;
        mock.set_default_response(ResponseTemplate::new("The search round is complete."))
            .await;

        for count in [0, 2] {
            let result = server
                .post("/v1/responses")
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model": model,
                    "input": "Exercise the configured search count.",
                    "tools": [{"type": "web_search", "count": count}],
                    "stream": stream,
                    "store": false
                }))
                .await;
            let response = completed_response(&result, stream);
            assert_eq!(
                output_text(&response)["text"],
                "The search round is complete."
            );
            {
                let params = provider.web_params.lock().unwrap();
                assert_eq!(params.len(), usize::from(count != 0));
                if count != 0 {
                    assert_eq!(params[0].count, Some(2));
                }
            }
            assert_no_history(&database, &response).await;
        }
        assert_eq!(mock.chat_completion_call_count(), 4);
    }
}

#[tokio::test]
async fn multiple_search_rounds_accumulate_sources_usage_and_unique_output_ids() {
    let provider = Arc::new(RecordingSearchProvider::default());
    let (server, database, mock) =
        setup_test_server_with_search_providers(provider.clone(), None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    for (round, query) in ["first", "second"].into_iter().enumerate() {
        mock.when(RequestMatcher::ToolResultCount(round))
            .respond_with(
                ResponseTemplate::new("")
                    .with_tool_calls(vec![ToolCall::new(
                        "web_search",
                        json!({"query": query}).to_string(),
                    )])
                    .with_cache_tokens(round as i32 + 1),
            )
            .await;
    }
    mock.set_default_response(
        ResponseTemplate::new("Both [s:0]first[/s:0] and [s:1]second[/s:1] sources agree.")
            .with_cache_tokens(3),
    )
    .await;

    for stream in [false, true] {
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model,
                "input": "Compare two searches.",
                "tools": [{"type": "web_search"}],
                "stream": stream,
                "store": false
            }))
            .await;
        let response = completed_response(&result, stream);
        let searches: Vec<&Value> = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "web_search_call")
            .collect();
        assert_eq!(searches.len(), 2);
        assert_ne!(searches[0]["id"], searches[1]["id"]);
        assert_eq!(searches[0]["action"]["query"], "first");
        assert_eq!(searches[1]["action"]["query"], "second");
        let text = output_text(&response);
        assert_eq!(text["text"], "Both first and second sources agree.");
        let annotations = text["annotations"].as_array().unwrap();
        assert_eq!(annotations.len(), 2);
        assert_citation(
            &annotations[0],
            text["text"].as_str().unwrap(),
            "first",
            "https://example.com/first",
        );
        assert_citation(
            &annotations[1],
            text["text"].as_str().unwrap(),
            "second",
            "https://example.com/second",
        );
        // Each tool turn reports 2 output tokens; the final answer reports 6.
        assert_eq!(response["usage"]["output_tokens"], 10);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            6
        );
        assert_eq!(
            response["usage"]["total_tokens"].as_u64().unwrap(),
            response["usage"]["input_tokens"].as_u64().unwrap() + 10
        );
        let params = mock.last_chat_params().await.unwrap();
        assert_eq!(params.messages[0].role, MessageRole::System);
        assert!(params.messages[1..]
            .iter()
            .all(|message| message.role != MessageRole::System));
        let results: Vec<_> = params
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .collect();
        assert_eq!(
            results.len(),
            2,
            "both search results must reach the final inference"
        );
        assert!(results[0]
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("https://example.com/first"));
        assert!(results[1]
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("https://example.com/second"));
        if stream {
            assert_search_lifecycle(&events(&result.text()), &response);
        }
        assert_no_history(&database, &response).await;
    }
    assert_eq!(mock.chat_completion_call_count(), 6);
    assert_eq!(provider.web_params.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn forced_search_choice_only_applies_until_the_search_has_executed() {
    for choice in [
        json!("required"),
        json!({"type": "function", "function": {"name": "web_search"}}),
    ] {
        let provider = Arc::new(RecordingSearchProvider::default());
        let (server, _, mock) =
            setup_test_server_with_search_providers(provider.clone(), None).await;
        *provider.model_provider.lock().unwrap() = Some(mock.clone());
        let model = setup_qwen_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        mock.when(RequestMatcher::ToolResultCount(0))
            .respond_with(
                ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
                    "web_search",
                    r#"{"query":"required-search"}"#,
                )]),
            )
            .await;
        mock.set_default_response(ResponseTemplate::new("The search supports this answer."))
            .await;

        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model,
                "input": "Search before answering.",
                "tools": [{"type": "web_search"}],
                "tool_choice": choice,
                "store": false,
                "stream": true
            }))
            .await;
        let response = completed_response(&result, true);
        assert_eq!(
            output_text(&response)["text"],
            "The search supports this answer."
        );
        assert_eq!(
            *provider.observed_tool_choices.lock().unwrap(),
            vec![choice]
        );
        let final_params = mock.last_chat_params().await.unwrap();
        assert_eq!(
            serde_json::to_value(final_params.tool_choice).unwrap(),
            "auto"
        );
        assert_eq!(mock.chat_completion_call_count(), 2);
        assert_eq!(provider.web_params.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn failed_search_emits_failure_with_usage_and_does_not_start_another_inference() {
    for tool in ["web_search", "web_context_search"] {
        let provider = Arc::new(RecordingSearchProvider {
            fail: true,
            ..Default::default()
        });
        let (server, database, mock) =
            setup_test_server_with_search_providers(provider.clone(), Some(provider.clone())).await;
        let model = setup_qwen_model(&server).await;
        let org = setup_org_with_credits(&server, 10_000_000_000).await;
        let api_key = get_api_key_for_org(&server, org.id).await;
        mock.set_default_response(
            ResponseTemplate::new("")
                .with_tool_calls(vec![ToolCall::new(tool, r#"{"query":"failure"}"#)]),
        )
        .await;
        for stream in [false, true] {
            let result = server.post("/v1/responses")
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({"model":model,"input":"Search.","tools":[{"type":tool}],"store":false,"stream":stream}))
                .await;
            assert_eq!(result.headers()["cache-control"], "no-store");
            if stream {
                assert_eq!(result.status_code(), 200);
                let events = events(&result.text());
                let failed = events.last().unwrap();
                assert_eq!(failed["type"], "response.failed");
                assert_eq!(failed["response"]["status"], "failed");
                assert_eq!(failed["response"]["error"]["type"], "internal_server_error");
                assert_eq!(failed["response"]["usage"]["output_tokens"], 2);
                assert!(events
                    .iter()
                    .any(|event| event["type"] == "response.web_search_call.searching"));
                assert!(events
                    .iter()
                    .all(|event| event["type"] != "response.completed"
                        && event["type"] != "response.web_search_call.completed"));
                assert_no_history(&database, &failed["response"]).await;
            } else {
                assert_eq!(result.status_code(), 500);
                assert_eq!(
                    result.json::<Value>()["error"]["type"],
                    "internal_server_error"
                );
            }
        }
        assert_eq!(mock.chat_completion_call_count(), 2);
        assert_eq!(
            provider.web_params.lock().unwrap().len()
                + provider.context_params.lock().unwrap().len(),
            2
        );
    }
}

#[tokio::test]
async fn repeated_searches_stop_at_the_request_round_limit() {
    let provider = Arc::new(RecordingSearchProvider::default());
    let (server, database, mock) =
        setup_test_server_with_search_providers(provider.clone(), None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.set_default_response(
        ResponseTemplate::new("")
            .with_tool_calls(vec![ToolCall::new("web_search", r#"{"query":"again"}"#)]),
    )
    .await;
    let result = server.post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({"model":model,"input":"Keep searching.","tools":[{"type":"web_search"}],"store":false,"stream":true}))
        .await;
    assert_eq!(result.status_code(), 200);
    let events = events(&result.text());
    let failed = events.last().unwrap();
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["error"]["type"], "internal_server_error");
    assert_eq!(mock.chat_completion_call_count(), 10);
    assert_eq!(provider.web_params.lock().unwrap().len(), 10);
    assert_eq!(failed["response"]["usage"]["output_tokens"], 20);
    assert!(events
        .iter()
        .all(|event| event["type"] != "response.completed"));
    assert_no_history(&database, &failed["response"]).await;
}

#[tokio::test]
async fn model_failure_after_search_retains_prior_usage_and_partial_output() {
    let provider = Arc::new(RecordingSearchProvider::default());
    let (server, database, mock) =
        setup_test_server_with_search_providers(provider.clone(), None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.when(RequestMatcher::ToolResultCount(0))
        .respond_with(
            ResponseTemplate::new("")
                .with_tool_calls(vec![ToolCall::new("web_search", r#"{"query":"first"}"#)])
                .with_cache_tokens(1),
        )
        .await;
    mock.set_default_response(
        ResponseTemplate::new("partial answer")
            .with_cache_tokens(2)
            .with_stream_error_after(
                1,
                inference_providers::CompletionError::HttpError {
                    status_code: 429,
                    message: "synthetic second-round rate limit".to_string(),
                    is_external: false,
                },
            ),
    )
    .await;
    for stream in [false, true] {
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "model": model,
                "input": "Answer after searching.",
                "tools": [{"type":"web_search"}],
                "store": false,
                "stream": stream
            }))
            .await;
        if stream {
            assert_eq!(result.status_code(), 200);
            let events = events(&result.text());
            let failed = events.last().unwrap();
            assert_eq!(failed["type"], "response.failed");
            let response = &failed["response"];
            assert_eq!(response["error"]["code"], "rate_limit_exceeded");
            assert_eq!(response["usage"]["output_tokens"], 3);
            assert_eq!(
                response["usage"]["input_tokens_details"]["cached_tokens"],
                3
            );
            assert_eq!(output_text(response)["text"], "partial");
            assert_eq!(response["output"][0]["type"], "web_search_call");
            assert_eq!(response["output"][0]["status"], "completed");
            assert!(events
                .iter()
                .all(|event| event["type"] != "response.completed"));
            assert_no_history(&database, response).await;
        } else {
            assert_eq!(result.status_code(), 429);
            assert_eq!(
                result.json::<Value>()["error"]["type"],
                "rate_limit_exceeded"
            );
        }
    }
    assert_eq!(mock.chat_completion_call_count(), 4);
    assert_eq!(provider.web_params.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn mixed_client_and_search_calls_return_control_without_executing_search() {
    let provider = Arc::new(RecordingSearchProvider::default());
    let (server, database, mock) =
        setup_test_server_with_search_providers(provider.clone(), None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let arguments = "{ \"path\" : \"notes.txt\" }";
    for stream in [false, true] {
        let request = json!({
            "model": model,
            "input": "Read local notes and search for references.",
            "tools": [
                {"type":"web_search"},
                {"type":"function","name":"read_local","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}
            ],
            "store": false,
            "stream": stream
        });
        // The adapter may alias client function names for the provider. The public
        // call below must still contain the original name and exact arguments.
        let (normalized, _) =
            services::responses::client_tools::normalize_request(request.clone()).unwrap();
        let provider_name = normalized["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["type"] == "function")
            .unwrap()["name"]
            .as_str()
            .unwrap();
        mock.set_default_response(ResponseTemplate::new("").with_tool_calls(vec![
            ToolCall::new("web_search", r#"{"query":"references"}"#),
            ToolCall::new(provider_name, arguments),
        ]))
        .await;
        let result = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .json(&request)
            .await;
        let response = completed_response(&result, stream);
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["name"], "read_local");
        assert_eq!(output[0]["arguments"], arguments);
        assert_eq!(output[0]["call_id"], "call_1");
        assert!(provider.web_params.lock().unwrap().is_empty());
        if stream {
            assert!(events(&result.text()).iter().all(|event| !event["type"]
                .as_str()
                .unwrap()
                .starts_with("response.web_search_call.")));
        }
        assert_no_history(&database, &response).await;
    }
    assert_eq!(mock.chat_completion_call_count(), 2);
}
