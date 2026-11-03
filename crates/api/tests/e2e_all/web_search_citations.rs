//! Deterministic citation coverage for stateless hosted Web Search.

use super::responses_hosted_search::{
    assert_citation, assert_no_history, assert_search_lifecycle, completed_response, events,
    output_text, RecordingSearchProvider,
};
use crate::common::*;
use inference_providers::{
    mock::{RequestMatcher, ResponseTemplate, ToolCall},
    MessageRole,
};
use serde_json::json;
use std::sync::Arc;

async fn check_web_search_citations(stream: bool) {
    let search = Arc::new(RecordingSearchProvider::default());
    let (server, database, mock) =
        setup_test_server_with_search_providers(search.clone(), None).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    mock.when(RequestMatcher::ToolResultCount(0))
        .respond_with(
            ResponseTemplate::new("")
                .with_tool_calls(vec![ToolCall::new(
                    "web_search",
                    json!({"query":"NEAR Protocol", "count":7}).to_string(),
                )])
                .with_cache_tokens(2),
        )
        .await;
    mock.set_default_response(
        ResponseTemplate::new("NEAR uses [s:0]sharded execution[/s:0].").with_cache_tokens(3),
    )
    .await;

    let result = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "input": "Search for source-backed facts about NEAR Protocol.",
            "stream": stream,
            "store": false,
            "tools": [{"type": "web_search", "count": 2}]
        }))
        .await;
    let response = completed_response(&result, stream);
    let output = response["output"].as_array().unwrap();
    let searches: Vec<_> = output
        .iter()
        .filter(|item| item["type"] == "web_search_call")
        .collect();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0]["status"], "completed");
    assert_eq!(searches[0]["action"]["type"], "search");
    assert_eq!(searches[0]["action"]["query"], "NEAR Protocol");
    let text = output_text(&response);
    assert_eq!(text["text"], "NEAR uses sharded execution.");
    let annotations = text["annotations"].as_array().unwrap();
    assert_eq!(
        annotations.len(),
        1,
        "the search result must produce a resolved citation"
    );
    assert_citation(
        &annotations[0],
        text["text"].as_str().unwrap(),
        "sharded execution",
        "https://example.com/NEAR-Protocol",
    );
    assert_eq!(annotations[0]["title"], "Source for NEAR Protocol");
    assert_eq!(mock.chat_completion_call_count(), 2);
    let params = mock.last_chat_params().await.unwrap();
    let result_message = params
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .unwrap();
    assert!(result_message
        .content
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Grounded context about NEAR Protocol."));
    assert_eq!(search.web_params.lock().unwrap().len(), 1);
    assert_eq!(search.web_params.lock().unwrap()[0].count, Some(2));
    // The mock reports 3 output tokens for the call and 4 for the final answer.
    assert_eq!(response["usage"]["output_tokens"], 7);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        5
    );
    if stream {
        let events = events(&result.text());
        assert_search_lifecycle(&events, &response);
        let deltas: String = events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect();
        assert_eq!(deltas, text["text"].as_str().unwrap());
        let added: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "response.output_text.annotation.added")
            .collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["annotation"], annotations[0]);
    }
    assert_no_history(&database, &response).await;
}

#[tokio::test]
async fn test_non_streaming_web_search_with_citations() {
    check_web_search_citations(false).await;
}

#[tokio::test]
async fn test_streaming_web_search_with_citations() {
    check_web_search_citations(true).await;
}
