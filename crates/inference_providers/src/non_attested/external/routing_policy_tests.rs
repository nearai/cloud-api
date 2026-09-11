use super::*;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn policy() -> Value {
    json!({
        "zdr": true,
        "only": ["fireworks"],
        "allow_fallbacks": false,
        "data_collection": "deny"
    })
}

fn provider(base_url: &str) -> ExternalProvider {
    ExternalProvider::new(ExternalProviderConfig {
        model_name: "test-model".into(),
        provider_config: serde_json::from_value(json!({
            "backend": "openai_compatible",
            "base_url": base_url,
            "extra_request_body": {
                "provider": {"zdr": false, "sort": "latency"},
                "other": "default"
            },
            "enforced_request_body": {"provider": policy()}
        }))
        .unwrap(),
        api_key: "synthetic-test-key".into(),
        timeout_seconds: 5,
    })
}

#[test]
fn mandatory_policy_overrides_defaults_and_user_fields() {
    let provider = provider("https://example.com");
    let mut extra = HashMap::from([
        (
            "provider".into(),
            json!({
                "zdr": false,
                "only": ["excluded-provider"],
                "allow_fallbacks": true,
                "data_collection": "allow",
                "sort": "price"
            }),
        ),
        ("other".into(), json!("user")),
    ]);
    provider.inject_extra_request_body(&mut extra);
    let mut expected = policy();
    expected["sort"] = json!("price");
    assert_eq!(extra["provider"], expected);
    assert_eq!(extra["other"], "user");
}

#[test]
fn mandatory_policy_replaces_malformed_provider_nodes() {
    let provider = provider("https://example.com");
    for value in [Value::Null, json!(false), json!(0), json!("off"), json!([])] {
        let mut extra = HashMap::from([("provider".into(), value)]);
        provider.inject_extra_request_body(&mut extra);
        assert_eq!(extra["provider"], policy());
    }
    let mut extra = HashMap::new();
    provider.inject_extra_request_body(&mut extra);
    let mut expected = policy();
    expected["sort"] = json!("latency");
    assert_eq!(extra["provider"], expected);
}

#[test]
fn mandatory_policy_merges_nested_objects_and_replaces_arrays() {
    let mut user = json!({"privacy": {"nested": {"enabled": false, "other": 1}}, "list": [1, 2]});
    merge_json_enforced(
        &mut user,
        &json!({"privacy": {"nested": {"enabled": true}}, "list": [3]}),
    );
    assert_eq!(
        user,
        json!({"privacy": {"nested": {"enabled": true, "other": 1}}, "list": [3]})
    );
}

#[test]
fn absent_policy_preserves_existing_user_precedence() {
    for enforcement in [Value::Null, json!({})] {
        let provider = ExternalProvider::new(ExternalProviderConfig {
            model_name: "test-model".into(),
            provider_config: serde_json::from_value(json!({
                "backend": "openai_compatible",
                "base_url": "https://example.com",
                "extra_request_body": {"provider": {"zdr": true}},
                "enforced_request_body": enforcement
            }))
            .unwrap(),
            api_key: "synthetic-test-key".into(),
            timeout_seconds: 5,
        });
        let mut extra = HashMap::from([("provider".into(), json!({"zdr": false}))]);
        provider.inject_extra_request_body(&mut extra);
        assert_eq!(extra["provider"], json!({"zdr": false}));
    }
}

#[test]
fn unsupported_or_ambiguous_policy_configurations_are_rejected() {
    for backend in ["anthropic", "gemini", "unknown"] {
        assert!(validate_enforced_request_body(&json!({
            "backend": backend, "enforced_request_body": {"provider": policy()}
        }))
        .is_err());
    }
    for enforced in [
        json!(false),
        json!([]),
        json!({"model": "override"}),
        json!({"messages": []}),
        json!({"provider": null}),
        json!({"provider": "off"}),
        json!({"provider": []}),
    ] {
        assert!(validate_enforced_request_body(&json!({
            "backend": "openai_compatible", "enforced_request_body": enforced
        }))
        .is_err());
    }
    for config in [
        json!({"backend": "anthropic"}),
        json!({"backend": "gemini", "enforced_request_body": null}),
        json!({"backend": "openai_compatible", "enforced_request_body": {}}),
        json!({"backend": "openai_compatible", "enforced_request_body": {"provider": policy()}}),
    ] {
        assert!(validate_enforced_request_body(&config).is_ok());
    }
}

#[tokio::test]
async fn multipart_operations_fail_before_sending_a_policy_free_request() {
    let server = MockServer::start().await;
    let provider = provider(&server.uri());
    let params = AudioTranscriptionParams {
        model: "test-model".into(),
        file_bytes: vec![1, 2, 3],
        filename: "synthetic.wav".into(),
        language: None,
        response_format: None,
        temperature: None,
        timestamp_granularities: None,
        extra: HashMap::new(),
    };
    assert!(matches!(
        provider.audio_transcription(params, String::new()).await,
        Err(AudioTranscriptionError::TranscriptionError(message)) if message.contains("mandatory routing")
    ));
    let params = ImageEditParams {
        model: "test-model".into(),
        prompt: "synthetic".into(),
        image: Arc::new(vec![1, 2, 3]),
        size: None,
        response_format: None,
    };
    assert!(matches!(
        provider.image_edit(Arc::new(params), String::new()).await,
        Err(ImageEditError::EditError(message)) if message.contains("mandatory routing")
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn assert_policy_on_wire(stream: bool) {
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
        .expect(7)
        .mount(&server)
        .await;
    let provider = provider(&server.uri());
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!("off"),
        json!([]),
        json!({"zdr": false, "only": ["excluded-provider"], "allow_fallbacks": true, "data_collection": "allow"}),
        json!({"zdr": null, "only": null, "allow_fallbacks": null, "data_collection": null}),
    ] {
        let params: ChatCompletionParams = serde_json::from_value(json!({
            "model": "test-model", "messages": [{"role": "user", "content": "19 + 23?"}],
            "provider": value, "max_tokens": 32, "stream": stream
        }))
        .unwrap();
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
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 7);
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        for (key, value) in policy().as_object().unwrap() {
            assert_eq!(&body["provider"][key], value);
        }
        assert_eq!(body["stream"], stream);
        assert_eq!(body["model"], "test-model");
    }
}

#[tokio::test]
async fn chat_request_cannot_override_policy_on_wire() {
    assert_policy_on_wire(false).await;
}

#[tokio::test]
async fn streaming_chat_request_cannot_override_policy_on_wire() {
    assert_policy_on_wire(true).await;
}
