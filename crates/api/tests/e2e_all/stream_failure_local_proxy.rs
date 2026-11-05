//! Opt-in HTTP contract: Cloud API -> local inference-proxy -> synthetic SSE engine.
//! Requires only PostgreSQL and a CPU-built proxy, with no live provider credentials.

use crate::common::*;
use axum::{body::Body, http::Response, routing::post, Json, Router};
use bytes::Bytes;
use futures_util::stream;
use serde_json::{json, Value};
use std::{io, process::Stdio, sync::Arc, time::Duration};

const BACKEND_TOKEN: &str = "synthetic-local-stream-backend-token";

async fn engine_stream(Json(request): Json<Value>) -> Response<Body> {
    let mode = request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let first = json!({"id": id, "object": "chat.completion.chunk", "created": 0,
        "model": E2E_QWEN_MODEL_NAME, "choices": [{"index": 0,
        "delta": {"role": "assistant", "content": "partial"}, "finish_reason": null}]});
    let finish = json!({"id": id, "object": "chat.completion.chunk", "created": 0,
        "model": E2E_QWEN_MODEL_NAME, "choices": [{"index": 0,
        "delta": {}, "finish_reason": "stop"}]});
    let usage = json!({"id": id, "object": "chat.completion.chunk", "created": 0,
        "model": E2E_QWEN_MODEL_NAME, "choices": [],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}});
    let first = Bytes::from(format!("data: {first}\n\n"));
    let finish = Bytes::from(format!(
        "data: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
    ));
    let body = stream::unfold(
        (0, mode, first, finish),
        |(step, mode, first, finish)| async move {
            let next = match step {
                0 => Ok(first.clone()),
                1 => {
                    // Let the initial delta reach the client before failing the body.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    match mode.as_str() {
                        "healthy" => Ok(finish.clone()),
                        "event_eof" => return None,
                        "body_disconnect" => Ok(Bytes::from_static(b"data: {\"id\":\"unfinished")),
                        // Keep the upstream open indefinitely: only the
                        // gateway read timeout may end this fixture. The
                        // client's longer timeout makes a missing watchdog fail.
                        "stall" => return std::future::pending().await,
                        _ => panic!("unknown fixture mode: {mode}"),
                    }
                }
                2 if mode == "body_disconnect" => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "synthetic upstream disconnect",
                    ))
                }
                _ => return None,
            };
            Some((next, (step + 1, mode, first, finish)))
        },
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

async fn request_stream(
    client: &reqwest::Client,
    url: reqwest::Url,
    api_key: &str,
    mode: &str,
) -> (String, Vec<Value>, bool) {
    let response = client
        .post(url)
        .bearer_auth(api_key)
        .json(&json!({"model": E2E_QWEN_MODEL_NAME,
            "messages": [{"role": "user", "content": mode}],
            "stream": true, "stream_options": {"include_usage": true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{mode}");
    assert!(response.headers().contains_key("x-request-id"), "{mode}");
    // A readable downstream must receive an SSE failure, not a broken HTTP body.
    let text = response
        .text()
        .await
        .expect("gateway SSE body must finish cleanly");
    let mut done = false;
    let mut events = Vec::new();
    let normalized = text.replace("\r\n", "\n");
    assert!(
        normalized.ends_with("\n\n"),
        "{mode}: unterminated SSE event: {text}"
    );
    for frame in normalized.split("\n\n").filter(|frame| !frame.is_empty()) {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| data.strip_prefix(' ').unwrap_or(data))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        assert!(!done, "{mode}: data followed the terminal marker: {text}");
        if data.trim() == "[DONE]" {
            done = true;
        } else {
            events.push(serde_json::from_str::<Value>(&data).expect("complete SSE JSON event"));
        }
    }
    let id = events
        .iter()
        .find_map(|event| event["id"].as_str())
        .expect("initial delta must survive")
        .to_owned();
    assert!(
        events
            .iter()
            .any(|event| event["choices"][0]["delta"]["content"] == "partial"),
        "{mode}: {text}"
    );
    (id, events, done)
}

#[tokio::test]
#[ignore = "requires INFERENCE_PROXY_TEST_BINARY pointing to a locally built inference-proxy"]
async fn interrupted_streams_fail_over_http_while_a_healthy_stream_completes() {
    let binary = std::env::var("INFERENCE_PROXY_TEST_BINARY")
        .expect("Set INFERENCE_PROXY_TEST_BINARY to an absolute inference-proxy binary path");
    assert!(std::path::Path::new(&binary).is_absolute());
    let engine = axum_test::TestServer::builder()
        .http_transport()
        .build(Router::new().route("/v1/chat/completions", post(engine_stream)));
    let (_, router, pool, _, database) = setup_test_server_with_pool_and_router().await;
    let server = axum_test::TestServer::builder()
        .http_transport()
        .build(router);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let proxy_url = format!("http://127.0.0.1:{port}");
    let mut proxy = tokio::process::Command::new(binary)
        .env_clear()
        .env("DEV", "true")
        .env("NON_TEE_DEPLOYMENT", "1")
        .env("MODEL_NAME", E2E_QWEN_MODEL_NAME)
        .env("TOKEN", BACKEND_TOKEN)
        .env(
            "VLLM_BASE_URL",
            engine
                .server_address()
                .unwrap()
                .as_str()
                .trim_end_matches('/'),
        )
        .env("VLLM_PROXY_STREAM_IDLE_TIMEOUT_SECS", "0")
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
    pool.register_provider(
        E2E_QWEN_MODEL_NAME.into(),
        Arc::new(inference_providers::attested::nearai::Provider::new(
            inference_providers::attested::nearai::Config {
                base_url: proxy_url,
                api_key: Some(BACKEND_TOKEN.into()),
                completion_timeout_seconds: 2,
                control_timeout_seconds: 2,
            },
        )),
    )
    .await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = server.server_url("/v1/chat/completions").unwrap();
    let (healthy, eof, disconnect, stall) = tokio::join!(
        request_stream(&client, url.clone(), &api_key, "healthy"),
        request_stream(&client, url.clone(), &api_key, "event_eof"),
        request_stream(&client, url.clone(), &api_key, "body_disconnect"),
        request_stream(&client, url, &api_key, "stall"),
    );
    let (healthy_id, events, done) = healthy;
    assert!(done);
    assert!(!events.iter().any(|event| event.get("error").is_some()));
    assert!(events
        .iter()
        .any(|event| event["choices"][0]["finish_reason"] == "stop"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["usage"].is_object())
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|event| event["usage"]["total_tokens"] == 6));

    let connection = database.pool().get().await.unwrap();
    for (mode, (id, events, done)) in [
        ("event_eof", eof),
        ("body_disconnect", disconnect),
        ("stall", stall),
    ] {
        assert!(
            !done,
            "{mode}: failed generation must not receive a success terminator"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.get("error").is_some())
                .count(),
            1,
            "{mode}: {events:?}"
        );
        assert!(
            events.last().unwrap().get("error").is_some(),
            "{mode}: error must be terminal"
        );
        assert!(
            !events.iter().any(|event| event["usage"].is_object()),
            "{mode}: no invented final usage"
        );
        assert!(
            !events.iter().any(
                |event| event["choices"].as_array().is_some_and(|choices| choices
                    .iter()
                    .any(|choice| !choice["finish_reason"].is_null()))
            ),
            "{mode}: no invented finish reason"
        );
        let count: i64 = connection
            .query_one(
                "SELECT COUNT(*) FROM chat_signatures WHERE chat_id = $1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            count, 0,
            "{mode}: interrupted generation must not gain a signature"
        );
        let count: i64 = connection
            .query_one(
                "SELECT COUNT(*) FROM organization_usage_log WHERE provider_request_id = $1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            count, 0,
            "{mode}: no usage was reported by the synthetic engine"
        );
    }
    let count: i64 = connection
        .query_one(
            "SELECT COUNT(*) FROM chat_signatures WHERE chat_id = $1",
            &[&healthy_id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        count > 0,
        "healthy generation must retain signature finalization"
    );
    proxy.kill().await.unwrap();
    proxy.wait().await.unwrap();
}
