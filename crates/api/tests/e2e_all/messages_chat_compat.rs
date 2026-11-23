use crate::common::*;
use bytes::Bytes;
use inference_providers::{
    mock::{RequestMatcher, ResponseTemplate, ToolCall},
    MessageRole,
};
use serde_json::{json, Value};

#[tokio::test]
async fn messages_runs_a_private_chat_tool_loop_and_bills_each_turn_once() {
    let (server, _, pool, database) =
        super::usage_admission::server(super::usage_admission::enforcing_settings()).await;
    let model = setup_qwen_model(&server).await;
    let mock = std::sync::Arc::new(
        inference_providers::mock::MockProvider::new_accept_all()
            .with_tier(inference_providers::ProviderTier::Near),
    );
    pool.register_provider(model.clone(), mock.clone()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    mock.when(RequestMatcher::ToolResultCount(0))
        .respond_with(
            ResponseTemplate::new("")
                .with_reasoning("Inspect the file")
                .with_tool_calls(vec![ToolCall::new("Read", r#"{"file_path":"sample.txt"}"#)
                    .with_thought_signature("opaque+/==")]),
        )
        .await;
    mock.when(RequestMatcher::ToolResultCount(1))
        .respond_with(
            ResponseTemplate::new("The file contains violet.").with_leading_control_events(2),
        )
        .await;
    let mut body = json!({"model":model, "system":[{"type":"text","text":"Help with files."}], "max_tokens":100,
        "thinking":{"type":"adaptive","display":"summarized"},
        "messages":[{"role":"user","content":"What is in sample.txt?"},
            {"role":"system","content":[{"type":"text","text":"Use the Read tool.","cache_control":{"type":"ephemeral"}}]}],
        "tools":[{"name":"Read","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}]});
    let post = |body: &Value| {
        server
            .post("/v1/messages?beta=true")
            .add_header("x-api-key", &key)
            .add_header("anthropic-version", "2023-06-01")
            .content_type("application/json")
            .bytes(Bytes::from(serde_json::to_string_pretty(body).unwrap()))
    };
    let first = post(&body).await;
    assert_eq!(first.status_code(), 200, "{}", first.text());
    let message: Value = first.json();
    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(message["content"][0]["thinking"], "Inspect the file");
    let tool = message["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap();
    assert_eq!(tool["name"], "Read");
    assert_eq!(tool["input"], json!({"file_path":"sample.txt"}));
    let first_id = message["id"].as_str().unwrap().to_owned();
    let source = first
        .header("x-serving-provider")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(source, "near");
    let mut receipts = vec![(
        first_id,
        serde_json::to_string_pretty(&body).unwrap(),
        first.text(),
        message["usage"].clone(),
    )];

    body["stream"] = json!(true);
    let history = body["messages"].as_array_mut().unwrap();
    history.push(json!({"role":"assistant","content":message["content"]}));
    history.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":tool["id"],"content":"violet"}]}));
    let second = post(&body).await;
    assert_eq!(second.status_code(), 200, "{}", second.text());
    let wire = second.text();
    let events: Vec<Value> = wire
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let second_id = events[0]["message"]["id"].as_str().unwrap().to_owned();
    assert_eq!(second.header("x-serving-provider"), source.as_str());
    assert_eq!(
        second.header("inference-id").to_str().unwrap(),
        services::completions::hash_inference_id_to_uuid(&second_id).to_string()
    );
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    let delta = events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    let text: String = events
        .iter()
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "The file contains violet.");
    receipts.push((
        second_id,
        serde_json::to_string_pretty(&body).unwrap(),
        wire,
        delta["usage"].clone(),
    ));
    let params = mock.last_chat_params().await.unwrap();
    assert_eq!(params.messages[2].role, MessageRole::System);
    assert_eq!(
        params.messages[2].content,
        Some(json!("Use the Read tool."))
    );
    let assistant = params
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("Inspect the file")
    );
    assert_eq!(
        assistant.tool_calls.as_ref().unwrap()[0]
            .thought_signature
            .as_deref(),
        Some("opaque+/==")
    );
    assert_eq!(params.messages.last().unwrap().role, MessageRole::Tool);
    assert_eq!(
        params.messages.last().unwrap().tool_call_id.as_deref(),
        tool["id"].as_str()
    );
    assert_eq!(
        params.messages.last().unwrap().content,
        Some(json!("violet"))
    );
    assert!(params
        .stream_options
        .as_ref()
        .unwrap()
        .include_usage
        .unwrap_or(false));

    let client = database.pool().get().await.unwrap();
    for (id, input, output, usage) in receipts {
        let receipt = server
            .get(&format!("/v1/signature/{id}?signing_algo=ecdsa"))
            .add_header("Authorization", format!("Bearer {key}"))
            .await;
        assert_eq!(receipt.status_code(), 200, "{}", receipt.text());
        let receipt: Value = receipt.json();
        assert_eq!(receipt["signature_kind"], "gateway");
        assert_eq!(
            receipt["text"],
            format!("{}:{}", compute_sha256(&input), compute_sha256(&output))
        );
        let inference_id = services::completions::hash_inference_id_to_uuid(&id);
        let rows = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let rows = client.query(
                    "SELECT input_tokens, output_tokens FROM organization_usage_log WHERE inference_id = $1",
                    &[&inference_id],
                ).await.unwrap();
                if !rows.is_empty() {
                    break rows;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get::<_, i32>("input_tokens"),
            usage["input_tokens"].as_i64().unwrap() as i32
        );
        assert_eq!(
            rows[0].get::<_, i32>("output_tokens"),
            usage["output_tokens"].as_i64().unwrap() as i32
        );
    }
    let count = server
        .post("/v1/messages/count_tokens")
        .add_header("x-api-key", &key)
        .add_header("anthropic-version", "2023-06-01")
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}))
        .await;
    assert_eq!(count.status_code(), 404);
    assert_eq!(count.json::<Value>()["error"]["type"], "not_found_error");
    assert_eq!(mock.chat_completion_call_count(), 2);
    super::usage_admission::assert_clean_settlements(
        &database,
        uuid::Uuid::parse_str(&org.id).unwrap(),
        2,
    )
    .await;
}

#[tokio::test]
async fn chat_backed_messages_return_a_billing_error_before_inference() {
    let (server, mock, _, _) =
        super::usage_admission::server(super::usage_admission::enforcing_settings()).await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 1).await;
    let key = get_api_key_for_org(&server, org.id).await;
    for stream in [false, true] {
        let response = server
            .post("/v1/messages")
            .add_header("x-api-key", &key)
            .add_header("anthropic-version", "2023-06-01")
            .json(&json!({"model":model,"max_tokens":16,"stream":stream,
                "messages":[{"role":"user","content":"Hello"}]}))
            .await;
        assert_eq!(response.status_code(), 402, "{}", response.text());
        assert_eq!(response.json::<Value>()["error"]["type"], "billing_error");
    }
    assert_eq!(mock.chat_completion_call_count(), 0);
}

#[tokio::test]
async fn openai_chat_uses_only_chat_completions_even_when_the_model_requires_responses() {
    use wiremock::{
        matchers::{body_partial_json, method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let upstream = MockServer::start().await;
    let model = format!("openai/messages-{}", uuid::Uuid::new_v4());
    let alias = format!("chat-alias-{}", uuid::Uuid::new_v4());
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let stops = ["<first>", "<second>"];
    let stream_chunk = json!({
        "id":format!("chatcmpl-{}",uuid::Uuid::new_v4()),"object":"chat.completion.chunk","created":1,"model":"chat-model",
        "choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":"stop","stop_reason":stops[1]}],
        "usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}
    });
    for (prompt, response) in [
        ("hello", ResponseTemplate::new(200).set_body_json(json!({"id":id,"object":"chat.completion","created":1,"model":"chat-model","choices":[
            {"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop","stop_reason":stops[1]}],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}))),
        ("stream", ResponseTemplate::new(200).set_body_raw(format!("data: {stream_chunk}\n\ndata: [DONE]\n\n"), "text/event-stream")),
        ("responses-only", ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"This model requires the Responses API","type":"invalid_request_error"}}))),
    ] {
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .and(body_partial_json(json!({"model":"chat-model","messages":[{"role":"user","content":prompt}]})))
            .respond_with(response).expect(1).mount(&upstream).await;
    }
    let (server, _, _, _) = setup_test_server_with_pool_and_config(|config| {
        config.external_providers.enable_anthropic_messages = true;
        config.external_providers.timeout_seconds = 5;
    })
    .await;
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(model.clone(),serde_json::from_value(json!({
        "inputCostPerToken":{"amount":1000,"currency":"USD"},"outputCostPerToken":{"amount":2000,"currency":"USD"},
        "modelDisplayName":"Chat fixture","modelDescription":"Chat model","aliases":[alias],"contextLength":10000,"maxOutputLength":512,
        "verifiable":false,"attestationSupported":false,"isActive":true,"inputModalities":["text"],"outputModalities":["text"],
        "supportedFeatures":["reasoning"],"providerType":"external","providerConfig":{"backend":"openai_compatible","base_url":format!("{}/v1",upstream.uri()),"model_name":"chat-model","api_key":"fixture-upstream-key"}
    })).unwrap());
    seed_models(&server, batch, get_session_id()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id).await;
    for (prompt, status) in [("hello", 200), ("stream", 200), ("responses-only", 400)] {
        let requested_model = if status == 200 { &alias } else { &model };
        let response = server
            .post("/v1/messages")
            .add_header("x-api-key", &key)
            .add_header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": requested_model,
                "thinking": {"type":"adaptive"},
                "output_config": {"effort":"high"},
                "max_tokens": 32,
                "stop_sequences": stops,
                "stream": prompt == "stream",
                "messages": [{"role":"user", "content":prompt}]
            }))
            .await;
        assert_eq!(response.status_code(), status, "{}", response.text());
        if prompt == "stream" {
            let events: Vec<Value> = response
                .text()
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let terminal = events
                .iter()
                .find(|e| e["type"] == "message_delta")
                .unwrap();
            assert_eq!(
                terminal["delta"],
                json!({"stop_reason":"stop_sequence","stop_sequence":stops[1]})
            );
            assert_eq!(events.last().unwrap()["type"], "message_stop");
            continue;
        }
        let message: Value = response.json();
        if status == 200 {
            assert_eq!(message["content"], json!([{"type":"text","text":"Hi"}]));
            assert_eq!(message["id"], id);
            assert_eq!(message["stop_reason"], "stop_sequence");
            assert_eq!(message["stop_sequence"], stops[1]);
            assert_eq!(
                response.header("x-model-alias-resolved").to_str().unwrap(),
                format!("{alias} -> {model}")
            );
            assert_eq!(response.header("x-serving-provider"), "non-attested");
        } else {
            assert_eq!(message["error"]["type"], "invalid_request_error");
        }
    }
    for request in upstream.received_requests().await.unwrap() {
        assert_ne!(request.url.path(), "/v1/responses");
        if request.url.path() == "/v1/chat/completions" {
            let body = request.body_json::<Value>().unwrap();
            assert!(body.get("reasoning_effort").is_none());
            assert_eq!(body["stop"], json!(stops));
        }
    }
    upstream.verify().await;
}
