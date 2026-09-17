use super::*;
use futures_util::TryStreamExt;
use serde_json::json;
use wiremock::{
    matchers::{body_json, header, method, path},
    Mock, MockServer, ResponseTemplate,
};

async fn backend() -> (MockServer, OpenAiCompatibleBackend, BackendConfig) {
    let server = MockServer::start().await;
    let backend = OpenAiCompatibleBackend {
        client: Client::builder()
            .no_proxy()
            .resolve("api.openai.com", *server.address())
            .build()
            .unwrap(),
    };
    let config = BackendConfig {
        base_url: format!("http://api.openai.com:{}/v1", server.address().port()),
        api_key: "test".into(),
        ..Default::default()
    };
    (server, backend, config)
}

#[tokio::test]
async fn native_responses_preserves_request_fields_and_response_bytes() {
    let (server, backend, config) = backend().await;
    let body = json!({"model":"openai/gpt-6-astra", "store":false,
        "input":[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]},
            {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":"sunny"}],
        "tools":[{"type":"function","name":"weather","strict":true,"parameters":{"type":"object","properties":{},"additionalProperties":false}}],
        "reasoning":{"effort":"high"}, "parallel_tool_calls":false, "include":["reasoning.encrypted_content"],
        "text":{"format":{"type":"json_object"}}, "future_field":{"preserved":true}});
    let mut expected = body.clone();
    expected["model"] = json!("gpt-6-astra");
    let bytes = "{ \"id\": \"resp_1\", \"output\": [{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}] }";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    let response = backend
        .responses_raw(&config, "gpt-6-astra", body)
        .await
        .unwrap();
    let chunks: Vec<_> = response.body.try_collect().await.unwrap();
    assert_eq!(chunks.concat(), bytes.as_bytes());
}

#[tokio::test]
async fn native_transport_rejects_stateful_requests_and_other_models() {
    let (server, backend, config) = backend().await;
    for body in [
        json!({}),
        json!({"store":true}),
        json!({"store":false,"previous_response_id":"resp_1"}),
        json!({"store":false,"conversation":"conv_1"}),
        json!({"store":false,"background":true}),
    ] {
        assert!(backend
            .responses_raw(&config, "gpt-6-astra", body)
            .await
            .is_err());
    }
    assert!(backend
        .responses_raw(&config, "gpt-5.6-sol", json!({"store":false}))
        .await
        .is_err());
    let other = BackendConfig {
        base_url: server.uri(),
        ..config
    };
    assert!(backend
        .responses_raw(&other, "gpt-6-astra", json!({"store":false}))
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn astra_chat_completions_still_use_native_chat_endpoint() {
    let (server, backend, config) = backend().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error":{"message":"Function tools require Responses"}})),
        )
        .expect(2)
        .mount(&server)
        .await;
    let params: ChatCompletionParams = serde_json::from_value(
        json!({"model":"gpt-6-astra","messages":[{"role":"user","content":"hi"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{}}}]}),
    )
    .unwrap();
    assert!(backend
        .chat_completion(&config, "gpt-6-astra", params.clone())
        .await
        .is_err());
    assert!(backend
        .chat_completion_stream(&config, "gpt-6-astra", params)
        .await
        .is_err());
}

#[tokio::test]
async fn native_transport_preserves_sse_and_http_errors() {
    let (server, backend, config) = backend().await;
    for (status, bytes, stream) in [
        (
            200,
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            true,
        ),
        (429, "{\"error\":{\"message\":\"Rate limited\"}}", false),
    ] {
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(body_json(
                json!({"model":"gpt-6-astra","store":false,"stream":stream}),
            ))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "2")
                    .set_body_raw(
                        bytes,
                        if stream {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let response = backend
            .responses_raw(
                &config,
                "gpt-6-astra",
                json!({"store":false,"stream":stream}),
            )
            .await
            .unwrap();
        assert_eq!(response.status.as_u16(), status);
        assert_eq!(response.headers["retry-after"], "2");
        assert_eq!(
            response
                .body
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .concat(),
            bytes.as_bytes()
        );
    }
}
