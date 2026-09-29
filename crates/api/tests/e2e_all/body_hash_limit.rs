//! `body_hash_middleware` runs outside API-key auth and buffers the request
//! body to hash it. Per-route `DefaultBodyLimit` is only enforced by axum
//! extractors, so it does not bound that buffering. These tests stream a
//! chunked body (no Content-Length) that counts how many bytes the server
//! actually pulls, and assert an unauthenticated oversize request is cut off
//! near the route limit instead of being read in full.

use crate::common::*;
use axum::body::Body;
use bytes::Bytes;
use http_body_util::BodyExt;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

const MIB: usize = 1024 * 1024;
const CHUNK: usize = MIB;

/// A body of `total` bytes, emitted in `CHUNK`-sized frames, that records how
/// many bytes the consumer has pulled.
fn counting_body(total: usize) -> (Body, Arc<AtomicUsize>) {
    let consumed = Arc::new(AtomicUsize::new(0));
    let counter = consumed.clone();
    let chunk = Bytes::from(vec![b'a'; CHUNK]);
    let stream = futures::stream::iter((0..total / CHUNK).map(move |_| {
        counter.fetch_add(CHUNK, Ordering::SeqCst);
        Ok::<_, std::io::Error>(chunk.clone())
    }));
    (Body::from_stream(stream), consumed)
}

async fn send_unauthenticated(
    router: axum::Router,
    uri: &str,
    total: usize,
) -> (axum::http::StatusCode, serde_json::Value, usize) {
    let (body, consumed) = counting_body(total);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("User-Agent", MOCK_USER_AGENT)
        .body(body)
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, consumed.load(Ordering::SeqCst))
}

fn assert_too_large_envelope(json: &serde_json::Value) {
    assert_eq!(
        json["error"]["type"], "invalid_request_error",
        "413 must use the OpenAI error envelope; got {json}"
    );
    assert!(json["error"]["message"].is_string(), "got {json}");
}

#[tokio::test]
async fn unauthenticated_oversize_chat_completion_is_not_fully_buffered() {
    let (_server, router) = setup_test_server_and_router().await;

    // 40 MiB against the 25 MiB text-inference limit.
    let total = 40 * MIB;
    let (status, json, consumed) =
        send_unauthenticated(router, "/v1/chat/completions", total).await;

    assert!(
        consumed <= 25 * MIB + CHUNK,
        "body_hash buffered {consumed} of {total} bytes before rejecting"
    );
    assert_eq!(status, 413);
    assert_too_large_envelope(&json);
}

#[tokio::test]
async fn unauthenticated_oversize_response_is_not_fully_buffered() {
    let (_server, router) = setup_test_server_and_router().await;

    // /v1/responses has no explicit DefaultBodyLimit, so axum's 2 MiB
    // extractor default is its effective limit.
    let total = 16 * MIB;
    let (status, json, consumed) = send_unauthenticated(router, "/v1/responses", total).await;

    assert!(
        consumed <= 2 * MIB + CHUNK,
        "body_hash buffered {consumed} of {total} bytes before rejecting"
    );
    assert_eq!(status, 413);
    assert_too_large_envelope(&json);
}

#[tokio::test]
async fn declared_oversize_content_length_is_rejected_without_reading() {
    let (_server, router) = setup_test_server_and_router().await;

    let total = 40 * MIB;
    let (body, consumed) = counting_body(total);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Content-Length", total.to_string())
        .header("User-Agent", MOCK_USER_AGENT)
        .body(body)
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), 413);
    assert_eq!(consumed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn under_limit_unauthenticated_request_still_reaches_auth() {
    let (_server, router) = setup_test_server_and_router().await;

    // Well under the 25 MiB cap: body_hash lets it through and auth rejects.
    let (status, _json, consumed) =
        send_unauthenticated(router, "/v1/chat/completions", 4 * MIB).await;

    assert_eq!(status, 401);
    assert_eq!(consumed, 4 * MIB);
}
