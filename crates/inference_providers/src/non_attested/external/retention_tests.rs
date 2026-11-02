use super::*;
use futures_util::StreamExt;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(base_url: &str) -> ExternalProvider {
    ExternalProvider::new(ExternalProviderConfig {
        model_name: "test-model".into(),
        provider_config: serde_json::from_value(json!({
            "backend": "openai_compatible",
            "base_url": base_url
        }))
        .unwrap(),
        api_key: "synthetic-test-key".into(),
        timeout_seconds: 5,
    })
}

/// What the Responses path builds (internal user id, `store: true` by
/// default, metadata kept with store) plus client copies in `extra`.
fn params_with_retention_fields(stream: bool) -> ChatCompletionParams {
    let mut params: ChatCompletionParams = serde_json::from_value(json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "19 + 23?"}],
        "max_tokens": 32,
        "stream": stream,
        "user": "00000000-0000-4000-8000-000000000001",
        "store": true,
        "metadata": {"ticket": "synthetic"}
    }))
    .unwrap();
    params.extra.insert("store".into(), json!(true));
    params
        .extra
        .insert("metadata".into(), json!({"client": "synthetic"}));
    params.original_request = Some(json!({"store": true, "metadata": {}, "user": "client"}));
    params
}

#[test]
fn strip_third_party_retention_clears_typed_extra_and_sidecar_fields() {
    let mut params = params_with_retention_fields(false);
    params.extra.insert("user".into(), json!("client-end-user"));

    params.strip_third_party_retention();

    assert!(params.user.is_none());
    assert!(params.store.is_none());
    assert!(params.metadata.is_none());
    assert!(!params.extra.contains_key("store"));
    assert!(!params.extra.contains_key("metadata"));
    // A client's own end-user id is theirs to send; only ours is removed.
    assert_eq!(params.extra["user"], "client-end-user");
    let original = params.original_request.as_ref().unwrap();
    assert!(original.get("store").is_none());
    assert!(original.get("metadata").is_none());
    assert_eq!(original["user"], "client");
}

async fn assert_no_retention_fields_on_wire(stream: bool) {
    let server = MockServer::start().await;
    let response = if stream {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("data: [DONE]\n\n")
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "synthetic", "object": "chat.completion", "created": 0,
            "model": "test-model", "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "42"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    let provider = provider(&server.uri());
    let params = params_with_retention_fields(stream);
    if stream {
        let chunks: Vec<_> = provider
            .chat_completion_stream(params, String::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(chunks.iter().all(Result::is_ok));
    } else {
        provider
            .chat_completion(params, String::new())
            .await
            .unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    for key in ["user", "store", "metadata"] {
        assert!(body.get(key).is_none(), "{key} reached the upstream");
    }
    assert_eq!(body["model"], "test-model");
}

#[tokio::test]
async fn chat_request_sends_no_user_store_or_metadata_upstream() {
    assert_no_retention_fields_on_wire(false).await;
}

#[tokio::test]
async fn streaming_chat_request_sends_no_user_store_or_metadata_upstream() {
    assert_no_retention_fields_on_wire(true).await;
}
