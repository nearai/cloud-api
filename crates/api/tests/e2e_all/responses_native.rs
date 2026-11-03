//! Canonical OpenAI models always use native stateless Responses.
use crate::common::*;
use bytes::Bytes;
use inference_providers::{mock::MockProvider, responses_raw::ResponsesRawResponse};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn native_stateless_astra_preserves_protocol_and_isolates_legacy_routes() {
    native_flow("openai/gpt-6-astra").await;
}

#[tokio::test]
async fn native_routing_supports_gpt41_without_model_exceptions() {
    native_flow("openai/gpt-4.1").await;
}

#[tokio::test]
async fn native_routing_supports_gpt54_without_model_exceptions() {
    native_flow("openai/gpt-5.4").await;
}

#[tokio::test]
async fn native_routing_supports_gpt61_sol_without_model_exceptions() {
    native_flow("openai/gpt-6.1-sol").await;
}

#[tokio::test]
async fn native_stateless_responses_reject_image_output_models() {
    let model = format!("openai/native-image-{}", uuid::Uuid::new_v4());
    let (server, _pool, _, _) = setup_test_server_with_pool().await;

    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(json!({
            "inputCostPerToken":{"amount":1000,"currency":"USD"},
            "outputCostPerToken":{"amount":2000,"currency":"USD"},
            "modelDisplayName":"Native image fixture",
            "modelDescription":"Must be rejected by Responses",
            "contextLength":10000,"maxOutputLength":1000,"isActive":true,"ownedBy":"openai",
            "verifiable":false,"inputModalities":["text"],"outputModalities":["image"]
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id).await;
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(&json!({"model":model,"store":false,"input":"draw a cat"}))
        .await;

    assert_eq!(response.status_code(), 400, "{}", response.text());
    assert!(response
        .text()
        .contains("Image generation and image editing"));
}

#[tokio::test]
async fn openai_responses_without_native_provider_never_fall_back_to_chat() {
    let model = format!("openai/chat-only-fixture-{}", uuid::Uuid::new_v4());
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(json!({
            "inputCostPerToken":{"amount":1000,"currency":"USD"},
            "outputCostPerToken":{"amount":2000,"currency":"USD"},
            "modelDisplayName":"Chat-only provider fixture",
            "modelDescription":"Native routing cannot silently use Chat",
            "contextLength":10000,"maxOutputLength":1000,"isActive":true,"ownedBy":"openai",
            "verifiable":false,"inputModalities":["text"],"outputModalities":["text"]
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let provider = Arc::new(MockProvider::new_accept_all());
    pool.register_provider(model.clone(), provider.clone())
        .await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id).await;
    for stream in [false, true] {
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&json!({"model":model,"input":"hello","stream":stream}))
            .await;
        assert_eq!(response.status_code(), 502, "{}", response.text());
        assert!(provider.last_chat_params().await.is_none());
    }
}

async fn native_flow(prefix: &str) {
    let model = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let alias = format!("native-alias-{}", uuid::Uuid::new_v4());
    let (server, pool, _, database) = setup_test_server_with_pool().await;
    let other = format!("other-{}", uuid::Uuid::new_v4());
    let other_alias = format!("openai/other-alias-{}", uuid::Uuid::new_v4());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let calls = captured.clone();
    let provider = Arc::new(MockProvider::new_accept_all().with_responses_handler(move |body: Value| {
        calls.lock().unwrap().push(body.clone());
        let native = json!({"id":format!("resp_{}", uuid::Uuid::new_v4()),"object":"response","status":"completed",
            "output":[{"type":"reasoning","id":"rs_native","summary":[],"encrypted_content":"opaque"},
            {"type":"function_call","id":"fc_native","call_id":"call_native","name":"weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}],
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":2}},
            "service_tier":"default","store":false});
        let stream = body["stream"] == true;
        let bytes = if stream { format!("event: response.completed\ndata: {}\n\n", json!({"type":"response.completed","response":native})) }
                    else { native.to_string() };
        ResponsesRawResponse {
            status: axum::http::StatusCode::OK,
            headers: [(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static(if stream {"text/event-stream"} else {"application/json"}))].into_iter().collect(),
            body: Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from(bytes))])),
        }
    }));
    let mut batch = api::models::BatchUpdateModelApiRequest::new();
    for name in [&model, &other] {
        batch.insert(
            name.clone(),
            serde_json::from_value(json!({
                "inputCostPerToken":{"amount":1000,"currency":"USD"},"outputCostPerToken":{"amount":2000,"currency":"USD"},
                "modelDisplayName":"Routing fixture","modelDescription":"Native Responses test",
                "contextLength":10000,"maxOutputLength":1000,"isActive":true,"ownedBy":"openai",
                "aliases":if name == &model {vec![alias.clone()]} else {vec![other_alias.clone()]},
                "verifiable":false,"inputModalities":["text"],"outputModalities":["text"]
            }))
            .unwrap(),
        );
    }
    admin_batch_upsert_models(&server, batch, get_session_id()).await;
    pool.register_provider(model.clone(), provider.clone())
        .await;
    pool.register_provider(other.clone(), provider.clone())
        .await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    let auth = format!("Bearer {key}");
    let mut request = json!({"model":alias,"store":false,"input":[
        {"type":"reasoning","id":"rs_prior","summary":[],"encrypted_content":"prior-opaque"},
        {"type":"function_call","call_id":"call_prior","name":"weather","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_prior","output":"sunny"}],
        "include":["reasoning.encrypted_content"],"parallel_tool_calls":false,
        "priority":999,"request_priority":999,
        "tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{},"additionalProperties":false},"strict":true}]});
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", &auth)
        .json(&request)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let native: Value = response.json();
    assert_eq!(native["output"][0]["encrypted_content"], "opaque");
    assert_eq!(native["output"][1]["call_id"], "call_native");
    let forwarded = captured.lock().unwrap()[0].clone();
    assert_eq!(forwarded["model"], model);
    assert!(forwarded.get("priority").is_none());
    assert!(forwarded.get("request_priority").is_none());
    for field in ["input", "include", "parallel_tool_calls", "tools", "store"] {
        assert_eq!(forwarded[field], request[field], "{field}");
    }
    assert!(
        !stored_response(&database, native["id"].as_str().unwrap()).await,
        "Stateless output must not be stored"
    );
    request["stream"] = json!(true);
    let streamed = server
        .post("/v1/responses")
        .add_header("Authorization", &auth)
        .json(&request)
        .await;
    assert_eq!(streamed.status_code(), 200, "{}", streamed.text());
    assert!(streamed.text().contains("event: response.completed\n"));
    assert!(streamed.text().contains("\"encrypted_content\":\"opaque\""));
    let streamed_event: Value = serde_json::from_str(
        streamed
            .text()
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    let native_ids = [
        native["id"].as_str().unwrap().to_owned(),
        streamed_event["response"]["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    assert_eq!(captured.lock().unwrap().len(), 2);

    // Invalid or stateful OpenAI requests must never switch to Chat.
    for rejected in [
        json!({"store":true}),
        json!({"store":"false"}),
        json!({"store":false,"previous_response_id":"resp_stale"}),
        json!({"store":false,"conversation":"conv_stale"}),
        json!({"store":false,"background":true}),
    ] {
        let mut body = json!({"model":model,"input":"hello"});
        body.as_object_mut()
            .unwrap()
            .extend(rejected.as_object().unwrap().clone());
        server
            .post("/v1/responses")
            .add_header("Authorization", &auth)
            .json(&body)
            .await
            .assert_status_bad_request();
    }
    assert_eq!(captured.lock().unwrap().len(), 2);
    assert!(provider.last_chat_params().await.is_none());

    // Both forms use native Responses and explicitly disable upstream storage.
    for null_store in [false, true] {
        let mut body = json!({"model":model,"input":"hello"});
        if null_store {
            body["store"] = Value::Null;
        }
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", &auth)
            .json(&body)
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let response: Value = response.json();
        assert_eq!(response["output"][0]["encrypted_content"], "opaque");
        assert_eq!(captured.lock().unwrap().last().unwrap()["store"], false);
        assert!(
            !stored_response(&database, response["id"].as_str().unwrap()).await,
            "Native stateless output must not be stored"
        );
    }
    assert_eq!(captured.lock().unwrap().len(), 4);
    assert!(provider.last_chat_params().await.is_none());

    // Codex Lite declarations are promoted while native grammars, custom
    // replay and opaque reasoning remain intact instead of becoming Chat tools.
    let tools = json!([
        {"type":"namespace","name":"functions","tools":[
            {"type":"function","name":"weather","parameters":{"type":"object"}}
        ]},
        {"type":"custom","name":"apply_patch","format":{
            "type":"grammar","syntax":"lark","definition":"start: /.+/s"
        }}
    ]);
    let history = json!([
        {"role":"user","content":"Apply the patch"},
        {"type":"reasoning","id":"rs_prior","summary":[],"encrypted_content":"prior-opaque"},
        {"type":"custom_tool_call","call_id":"call_patch","name":"apply_patch","input":"raw patch"},
        {"type":"custom_tool_call_output","call_id":"call_patch","output":"Done"}
    ]);
    let mut input = vec![json!({"type":"additional_tools","role":"developer","tools":tools})];
    input.extend(history.as_array().unwrap().iter().cloned());
    let lite = server
        .post("/v1/responses")
        .add_header("Authorization", &auth)
        .json(&json!({"model":alias,"input":input,"reasoning":{"effort":"high"},"include":["reasoning.encrypted_content"]}))
        .await;
    assert_eq!(lite.status_code(), 200, "{}", lite.text());
    let forwarded = captured.lock().unwrap().last().unwrap().clone();
    assert_eq!(forwarded["model"], model);
    assert_eq!(forwarded["store"], false);
    assert_eq!(forwarded["tools"], tools);
    assert_eq!(forwarded["input"], history);
    assert_eq!(forwarded["reasoning"], json!({"effort":"high"}));
    assert_eq!(forwarded["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(captured.lock().unwrap().len(), 5);
    assert!(provider.last_chat_params().await.is_none());

    // Routing uses the resolved canonical name, not ownedBy or alias spelling.
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", &auth)
        .json(&json!({"model":other_alias,"store":false,"input":"hello"}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(captured.lock().unwrap().len(), 5);
    assert!(provider.last_chat_params().await.is_some());
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", &auth)
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(captured.lock().unwrap().len(), 5);
    assert!(provider.last_chat_params().await.is_some());

    // Poll async streaming usage; native records must not point at stored response rows.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = server
                .get(&format!(
                    "/v1/organizations/{}/usage/history?limit=100",
                    org.id
                ))
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(history.status_code(), 200, "{}", history.text());
            let history: api::routes::usage::UsageHistoryResponse = history.json();
            let native: Vec<_> = history
                .data
                .iter()
                .filter(|e| {
                    e.provider_request_id
                        .as_ref()
                        .is_some_and(|id| native_ids.contains(id))
                })
                .collect();
            if native.len() == 2 {
                for entry in native {
                    assert!(entry.response_id.is_none());
                    assert_eq!(entry.cache_read_tokens, 2);
                    assert_eq!(entry.input_tokens, 10);
                    assert_eq!(entry.output_tokens, 5);
                    assert_eq!(entry.stop_reason.as_deref(), Some("completed"));
                    assert!(entry.total_cost > 0);
                }
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("native usage recorded");
}

async fn stored_response(database: &database::Database, id: &str) -> bool {
    let id = uuid::Uuid::parse_str(id.strip_prefix("resp_").unwrap_or(id)).unwrap();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM responses WHERE id = $1)",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}
