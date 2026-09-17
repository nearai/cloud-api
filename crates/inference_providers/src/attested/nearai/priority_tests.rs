use super::*;
use futures_util::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn params(priority: i32) -> ChatCompletionParams {
    let mut params: ChatCompletionParams = serde_json::from_value(json!({
        "model": "synthetic-model",
        "messages": [{"role": "user", "content": "synthetic input"}],
        "request_priority": 999,
        "priority": 999,
        "temperature": 0.5
    }))
    .unwrap();
    assert_eq!(
        params.request_priority, 0,
        "JSON cannot set internal metadata"
    );
    params.request_priority = priority;
    params
}

#[tokio::test]
async fn priority_is_a_per_request_header_for_json_and_sse() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                let chunk = json!({"id": "synthetic", "object": "chat.completion.chunk", "created": 0,
                    "model": "synthetic-model", "choices": [{"index": 0,
                    "delta": {"content": "priority-ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}});
                ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "synthetic", "object": "chat.completion", "created": 0,
                    "model": "synthetic-model", "choices": [{"index": 0,
                    "message": {"role": "assistant", "content": "priority-ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        }).expect(4).mount(&upstream).await;
    let provider = Provider::new(Config {
        base_url: upstream.uri(),
        api_key: Some("synthetic-backend-token".into()),
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    });
    // Simultaneous requests share the provider/client but carry distinct policy.
    let (low, default) = tokio::join!(
        provider.chat_completion(params(-2), "synthetic-hash-low".into()),
        provider.chat_completion(params(0), "synthetic-hash-default".into())
    );
    for response in [low, default] {
        assert_eq!(
            response.unwrap().response.choices[0]
                .message
                .content
                .as_deref(),
            Some("priority-ok")
        );
    }
    for priority in [-2, 0] {
        let stream = provider
            .chat_completion_stream(params(priority), "synthetic-hash-stream".into())
            .await
            .unwrap();
        let events: Vec<_> = stream.try_collect().await.unwrap();
        assert!(events
            .iter()
            .any(|event| String::from_utf8_lossy(&event.raw_bytes).contains("priority-ok")));
    }
    let requests = upstream.received_requests().await.unwrap();
    let mut priorities: Vec<_> = requests
        .iter()
        .map(|request| request.headers["x-nearai-priority"].to_str().unwrap())
        .collect();
    priorities.sort();
    assert_eq!(priorities, ["-2", "-2", "0", "0"]);
    for request in requests {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body.get("priority").is_none());
        assert!(body.get("request_priority").is_none());
        assert_eq!(body["temperature"], 0.5);
        assert!(request.headers["x-request-hash"]
            .to_str()
            .unwrap()
            .starts_with("synthetic-hash-"));
        assert_eq!(
            request.headers["authorization"],
            "Bearer synthetic-backend-token"
        );
    }
}

#[test]
fn internal_priority_is_never_serialized() {
    let mut params: ChatCompletionParams = serde_json::from_value(json!({
        "model": "synthetic-model",
        "messages": [{"role": "user", "content": "synthetic input"}]
    }))
    .unwrap();
    params.request_priority = -2;
    let serialized = serde_json::to_value(&params).unwrap();
    assert!(serialized.get("request_priority").is_none());
    assert!(serialized.get("priority").is_none());
    assert_eq!(params.clone().request_priority, -2);
}

#[tokio::test]
async fn external_provider_does_not_receive_internal_priority() {
    use crate::non_attested::external::backend::{BackendConfig, ExternalBackend};
    use crate::non_attested::external::openai_compatible::OpenAiCompatibleBackend;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "synthetic", "object": "chat.completion", "created": 0,
            "model": "synthetic-model", "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let config = BackendConfig {
        base_url: upstream.uri(),
        api_key: "synthetic".into(),
        ..Default::default()
    };
    let mut request: ChatCompletionParams = serde_json::from_value(json!({
        "model": "synthetic-model", "messages": [{"role": "user", "content": "synthetic"}],
        "service_tier": "priority"
    }))
    .unwrap();
    request.request_priority = -2;
    OpenAiCompatibleBackend::new()
        .chat_completion(&config, "synthetic-model", request)
        .await
        .unwrap();
    let requests = upstream.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("x-nearai-priority"));
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("request_priority").is_none());
    assert!(body.get("priority").is_none());
    assert_eq!(body["service_tier"], "priority");
}
