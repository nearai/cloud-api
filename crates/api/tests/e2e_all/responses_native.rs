//! Native Astra routing must remain an explicit stateless opt-in.
use crate::common::*;
use bytes::Bytes;
use inference_providers::{mock::MockProvider, responses_raw::ResponsesRawResponse};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn native_stateless_astra_preserves_protocol_and_isolates_legacy_routes() {
    native_flow("openai/gpt-6-astra", true).await;
}

#[tokio::test]
async fn native_routing_supports_explicitly_configured_non_astra_models() {
    native_flow("configured/custom-model", true).await;
}

#[tokio::test]
async fn native_routing_is_disabled_when_allowlist_is_empty() {
    native_flow("openai/gpt-6-astra", false).await;
}

async fn native_flow(prefix: &str, enabled: bool) {
    let model = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let alias = format!("native-alias-{}", uuid::Uuid::new_v4());
    let (server, pool, _, database) = setup_test_server_with_pool_and_config(|config| {
        config.native_responses_models = if enabled { vec![model.clone()] } else { vec![] };
    })
    .await;
    let other = format!("other-{}", uuid::Uuid::new_v4());
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
                "aliases":if name == &model {vec![alias.clone()]} else {vec![]},
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
    if !enabled {
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", &auth)
            .json(&json!({"model":model,"store":false,"input":"hello"}))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(captured.lock().unwrap().is_empty());
        assert!(provider.last_chat_params().await.is_some());
        return;
    }
    let mut request = json!({"model":alias,"store":false,"input":[
        {"type":"reasoning","id":"rs_prior","summary":[],"encrypted_content":"prior-opaque"},
        {"type":"function_call","call_id":"call_prior","name":"weather","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_prior","output":"sunny"}],
        "include":["reasoning.encrypted_content"],"parallel_tool_calls":false,
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

    // Both explicit and default storage must still persist local response records.
    for store in [Some(true), None] {
        let mut legacy = json!({"model":model,"input":"hello"});
        if let Some(store) = store {
            legacy["store"] = json!(store);
        }
        let response = server
            .post("/v1/responses")
            .add_header("Authorization", &auth)
            .json(&legacy)
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let value: Value = response.json();
        assert!(stored_response(&database, value["id"].as_str().unwrap()).await);
        let followup = server.post("/v1/responses")
            .add_header("Authorization", &auth)
            .json(&json!({"model":model,"store":false,"previous_response_id":value["id"],"input":"follow up"}))
            .await;
        assert_eq!(followup.status_code(), 200, "{}", followup.text());
        assert_eq!(captured.lock().unwrap().len(), 2);
    }
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", &auth)
        .json(&json!({"model":other,"store":false,"input":"hello"}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", &auth)
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(captured.lock().unwrap().len(), 2);
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
