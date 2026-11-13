use super::*;
use crate::UpstreamCacheSalt;
use futures_util::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLIENT_SALT: &str = "synthetic-client-salt";
const SERVER_SALT: &str = "synthetic-server-namespace";

fn params(server_salt: Option<&str>) -> ChatCompletionParams {
    let mut params: ChatCompletionParams = serde_json::from_value(json!({
        "model": "synthetic-model",
        "messages": [{"role": "user", "content": "synthetic input"}],
        "cache_salt": CLIENT_SALT,
        "temperature": 0.5
    }))
    .unwrap();
    assert!(
        params.upstream_cache_salt.is_none(),
        "JSON cannot set the server namespace"
    );
    params.upstream_cache_salt = server_salt.map(|salt| UpstreamCacheSalt::new(salt.into()));
    params
}

fn respond(request: &wiremock::Request) -> ResponseTemplate {
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    if body["stream"] == true {
        let chunk = json!({"id": "synthetic", "object": "chat.completion.chunk", "created": 0,
            "model": "synthetic-model", "choices": [{"index": 0,
            "delta": {"content": "salt-ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "synthetic", "object": "chat.completion", "created": 0,
            "model": "synthetic-model", "choices": [{"index": 0,
            "message": {"role": "assistant", "content": "salt-ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    }
}

async fn fleet_bodies(server_salt: Option<&str>) -> Vec<serde_json::Value> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(respond)
        .expect(2)
        .mount(&upstream)
        .await;
    let provider = Provider::new(Config {
        base_url: upstream.uri(),
        api_key: Some("synthetic-backend-token".into()),
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    });
    let response = provider
        .chat_completion(params(server_salt), "synthetic-hash-json".into())
        .await
        .unwrap();
    assert_eq!(
        response.response.choices[0].message.content.as_deref(),
        Some("salt-ok")
    );
    let stream = provider
        .chat_completion_stream(params(server_salt), "synthetic-hash-sse".into())
        .await
        .unwrap();
    let events: Vec<_> = stream.try_collect().await.unwrap();
    assert!(events
        .iter()
        .any(|event| String::from_utf8_lossy(&event.raw_bytes).contains("salt-ok")));
    upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn fleet_body_carries_the_server_namespace_not_the_client_salt() {
    let bodies = fleet_bodies(Some(SERVER_SALT)).await;
    assert_eq!(bodies.len(), 2);
    for body in bodies {
        assert_eq!(body["cache_salt"], SERVER_SALT);
        assert_eq!(body["temperature"], 0.5);
    }
}

#[tokio::test]
async fn fleet_body_drops_a_client_salt_without_a_server_namespace() {
    let bodies = fleet_bodies(None).await;
    assert_eq!(bodies.len(), 2);
    for body in bodies {
        assert!(body.get("cache_salt").is_none());
    }
}

#[test]
fn server_namespace_is_never_serialized_directly() {
    let mut request = params(Some(SERVER_SALT));
    request.extra.remove("cache_salt");
    let serialized = serde_json::to_value(&request).unwrap();
    assert!(serialized.get("cache_salt").is_none());
    assert!(serialized.get("upstream_cache_salt").is_none());
    assert!(!format!("{request:?}").contains(SERVER_SALT));
}
