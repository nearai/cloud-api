//! Opt-in, CPU-only contract test: real HTTP Cloud API -> real inference-proxy
//! process -> synthetic OpenAI engine. PostgreSQL is real; no GPU/TEE is required.
use super::*;
use std::{process::Stdio, sync::Arc, time::Duration};
use wiremock::{
    matchers::{body_string_contains, method, path},
    Mock, MockServer, ResponseTemplate,
};

const BACKEND_TOKEN: &str = "synthetic-local-priority-backend-token";
const OUTPUT: &str = "local-priority-ok";

fn engine_response(request: &wiremock::Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let usage = json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8});
    if body["stream"] == true {
        let chunk = json!({"id": id, "object": "chat.completion.chunk", "created": 0,
            "model": E2E_QWEN_MODEL_NAME, "choices": [{"index": 0,
                "delta": {"role": "assistant", "content": OUTPUT}, "finish_reason": "stop"}],
            "usage": usage});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": id, "object": "chat.completion", "created": 0,
            "model": E2E_QWEN_MODEL_NAME, "choices": [{"index": 0,
                "message": {"role": "assistant", "content": OUTPUT}, "finish_reason": "stop"}],
            "usage": usage
        }))
    }
}

async fn engine_requests(engine: &MockServer) -> Vec<wiremock::Request> {
    engine
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .collect()
}

fn assert_completion(response: &axum_test::TestResponse, endpoint: &str, stream: bool) {
    response.assert_status_ok();
    assert!(
        response.text().contains(OUTPUT),
        "{endpoint}: {}",
        response.text()
    );
    if stream {
        assert!(response.text().contains(if endpoint == "responses" {
            "response.completed"
        } else {
            "[DONE]"
        }));
        assert!(!response.text().contains("response.failed"));
    } else if endpoint == "responses" {
        assert_eq!(response.json::<Value>()["status"], "completed");
    } else {
        assert_eq!(
            response.json::<Value>()["choices"][0]["finish_reason"],
            "stop"
        );
        assert_eq!(response.json::<Value>()["usage"]["total_tokens"], 8);
    }
}

#[tokio::test]
#[ignore = "requires INFERENCE_PROXY_TEST_BINARY pointing to a locally built inference-proxy"]
async fn priority_reaches_local_proxy_and_engine_over_http() {
    let binary = std::env::var("INFERENCE_PROXY_TEST_BINARY")
        .expect("Set INFERENCE_PROXY_TEST_BINARY to an absolute inference-proxy binary path");
    assert!(std::path::Path::new(&binary).is_absolute());
    let engine = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": E2E_QWEN_MODEL_NAME,
                "object": "model", "created": 0, "owned_by": "synthetic"}]
        })))
        .mount(&engine)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(engine_response)
        .expect(36)
        .mount(&engine)
        .await;

    let (_, router, pool, _, _) = setup_test_server_with_pool_and_router().await;
    let server = axum_test::TestServer::builder()
        .http_transport()
        .build(router);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let proxy_url = format!("http://127.0.0.1:{port}");
    // No inherited credentials, endpoints, or .env configuration. The process
    // is killed even when an assertion fails, and all listeners are loopback.
    let mut proxy = tokio::process::Command::new(binary)
        .env_clear()
        .env("DEV", "true")
        .env("NON_TEE_DEPLOYMENT", "1")
        .env("MODEL_NAME", E2E_QWEN_MODEL_NAME)
        .env("TOKEN", BACKEND_TOKEN)
        .env("VLLM_BASE_URL", engine.uri())
        .env("LISTEN_ADDR", "127.0.0.1")
        .env("LISTEN_PORT", port.to_string())
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                proxy.try_wait().unwrap().is_none(),
                "local proxy exited at startup"
            );
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("local proxy did not start");

    let provider = inference_providers::attested::nearai::Provider::new(
        inference_providers::attested::nearai::Config {
            base_url: proxy_url,
            api_key: Some(BACKEND_TOKEN.into()),
            completion_timeout_seconds: 10,
            control_timeout_seconds: 2,
        },
    );
    pool.register_provider(E2E_QWEN_MODEL_NAME.into(), Arc::new(provider))
        .await;
    let org = setup_org_with_credits(&server, 100_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;

    // The same already-used API key must see admin updates immediately, even
    // when the caller supplies conflicting header and body values.
    for priority in [0, -2, -1000, 1000, 0] {
        set_priority(&server, &org.id, priority).await;
        for endpoint in ["chat/completions", "completions", "responses"] {
            for stream in [false, true] {
                let mut body = json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
                    "priority": 999, "request_priority": 999});
                match endpoint {
                    "chat/completions" => {
                        body["messages"] =
                            json!([{"role": "user", "content": "synthetic local test"}])
                    }
                    "completions" => body["prompt"] = json!("synthetic local test"),
                    _ => body["input"] = json!("synthetic local test"),
                }
                let before = engine_requests(&engine).await.len();
                let response = server
                    .post(&format!("/v1/{endpoint}"))
                    .add_header("Authorization", format!("Bearer {key}"))
                    .add_header("X-NearAI-Priority", "999")
                    .json(&body)
                    .await;
                assert_completion(&response, endpoint, stream);
                let requests = engine_requests(&engine).await;
                assert_eq!(requests.len(), before + 1);
                let forwarded: Value = serde_json::from_slice(&requests[before].body).unwrap();
                assert_eq!(
                    forwarded["priority"], priority,
                    "{endpoint}, stream={stream}"
                );
                assert!(forwarded.get("request_priority").is_none());
            }
        }
    }

    let other = setup_org_with_credits(&server, 100_000_000_000).await;
    let other_key = get_api_key_for_org(&server, other.id).await;
    set_priority(&server, &org.id, -2).await;
    for stream in [false, true] {
        let before = engine_requests(&engine).await.len();
        let body = json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
            "messages": [{"role": "user", "content": "synthetic concurrent local test"}]});
        let first = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&body);
        let second = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {other_key}"))
            .json(&body);
        let (first, second) = tokio::join!(first, second);
        assert_completion(&first, "chat/completions", stream);
        assert_completion(&second, "chat/completions", stream);
        let requests = engine_requests(&engine).await;
        let mut seen: Vec<i64> = requests[before..]
            .iter()
            .map(|request| {
                serde_json::from_slice::<Value>(&request.body).unwrap()["priority"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        seen.sort();
        assert_eq!(seen, [-2, 0]);
    }

    // A transient engine rejection must preserve the captured policy when the
    // provider pool retries, and the eventual response must still complete.
    for stream in [false, true] {
        let prompt = format!("synthetic retry stream={stream}");
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(&prompt))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "error": {"message": "Synthetic temporary unavailability", "type": "server_error"}
            })))
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&engine)
            .await;
        let before = engine_requests(&engine).await.len();
        let response = server
            .post("/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
                "messages": [{"role": "user", "content": prompt}]}))
            .await;
        assert_completion(&response, "chat/completions", stream);
        let requests = engine_requests(&engine).await;
        assert_eq!(
            requests.len(),
            before + 2,
            "one rejection followed by one successful retry"
        );
        for request in &requests[before..] {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["priority"], -2);
        }
    }
    assert_eq!(engine_requests(&engine).await.len(), 38);
    proxy.kill().await.unwrap();
    proxy.wait().await.unwrap();
}
