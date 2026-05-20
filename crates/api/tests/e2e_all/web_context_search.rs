//! E2E coverage for the Responses API `web_context_search` tool.
//!
//! Uses mock inference and mock context-search providers so CI does not need Brave credentials.

use crate::common::{
    get_api_key_for_org, mock_prompts, setup_org_with_credits, setup_qwen_model,
    setup_test_server_with_search_providers, MockWebContextSearchProvider, MockWebSearchProvider,
};
use inference_providers::mock::{RequestMatcher, ResponseTemplate, ToolCall};
use serde_json::json;
use services::responses::tools::WebSearchResult;
use std::sync::Arc;

#[tokio::test]
async fn test_non_streaming_web_context_search_with_mock_provider() {
    let context_provider = Arc::new(MockWebContextSearchProvider::new(vec![WebSearchResult {
        title: "NEAR Context Source".to_string(),
        url: "https://example.com/near-context".to_string(),
        snippet: "NEAR Protocol context from Brave LLM Context search.".to_string(),
    }]));
    let captured_context_params = context_provider.last_params();

    let (server, _database, mock) = setup_test_server_with_search_providers(
        Arc::new(MockWebSearchProvider::default_results()),
        Some(context_provider),
    )
    .await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let user_prompt = "Use context search to ground a short answer about NEAR Protocol.";
    let expected_prompt = mock_prompts::build_prompt(user_prompt);

    mock.when(RequestMatcher::PromptWithTools {
        prompt: expected_prompt,
        tool_names: vec!["web_context_search".to_string()],
    })
    .respond_with(
        ResponseTemplate::new("").with_tool_calls(vec![ToolCall::new(
            "web_context_search",
            json!({
                "query": "NEAR Protocol",
                "country": "US",
                "search_lang": "en",
                "freshness": "pw",
                "spellcheck": false,
                "count": 5,
                "maximum_number_of_urls": 2,
                "maximum_number_of_tokens": 2048,
                "maximum_number_of_snippets": 4,
                "maximum_number_of_tokens_per_url": 1024,
                "maximum_number_of_snippets_per_url": 2,
                "context_threshold_mode": "strict"
            })
            .to_string(),
        )]),
    )
    .await;

    mock.set_default_response(ResponseTemplate::new(
        "NEAR Protocol is a sharded layer-one blockchain with source-backed context [s:0]from Brave Context[/s:0].",
    ))
    .await;

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&json!({
            "model": model,
            "input": user_prompt,
            "stream": false,
            "tools": [
                {
                    "type": "web_context_search"
                }
            ]
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "response failed: {}",
        response.text()
    );

    let response_body = response.json::<serde_json::Value>();
    assert_eq!(response_body["status"], "completed");

    let output = response_body["output"]
        .as_array()
        .expect("response output should be an array");

    let web_search_call = output
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("web_context_search should emit a web_search_call output item");
    assert_eq!(web_search_call["status"], "completed");
    assert_eq!(web_search_call["action"]["type"], "search");
    assert_eq!(web_search_call["action"]["query"], "NEAR Protocol");

    let final_output_text = output
        .iter()
        .rev()
        .find(|item| item["type"] == "message")
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.iter().find(|part| part["type"] == "output_text"))
        .expect("final response should contain output_text");

    let text = final_output_text["text"]
        .as_str()
        .expect("output_text should include text");
    assert!(text.contains("source-backed context"));
    assert!(
        !text.contains("[s:0]"),
        "citation tags should be stripped from final text"
    );

    let annotations = final_output_text["annotations"]
        .as_array()
        .expect("output_text should include annotations");
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0]["type"], "url_citation");
    assert_eq!(annotations[0]["title"], "NEAR Context Source");
    assert_eq!(annotations[0]["url"], "https://example.com/near-context");

    let params = captured_context_params
        .lock()
        .expect("mock context search params lock poisoned")
        .clone()
        .expect("context provider should have been called");
    assert_eq!(params.query, "NEAR Protocol");
    assert_eq!(params.country.as_deref(), Some("US"));
    assert_eq!(params.search_lang.as_deref(), Some("en"));
    assert_eq!(params.freshness.as_deref(), Some("pw"));
    assert_eq!(params.spellcheck, Some(false));
    assert_eq!(params.count, Some(5));
    assert_eq!(params.maximum_number_of_urls, Some(2));
    assert_eq!(params.maximum_number_of_tokens, Some(2048));
    assert_eq!(params.maximum_number_of_snippets, Some(4));
    assert_eq!(params.maximum_number_of_tokens_per_url, Some(1024));
    assert_eq!(params.maximum_number_of_snippets_per_url, Some(2));
    assert_eq!(params.context_threshold_mode.as_deref(), Some("strict"));
}
