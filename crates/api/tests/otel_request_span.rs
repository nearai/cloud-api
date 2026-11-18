//! The request span exported for an HTTP request: its name and attributes
//! carry only the route template and allowlisted fields, it stays open until
//! the response body is finished, and an inbound `traceparent` is ignored.

use api::middleware::request_correlation_middleware;
use api::telemetry::{tracer_provider_with_processor, SPAN_ATTRIBUTE_ALLOWLIST};
use axum::{body::Body, http::Request, middleware::from_fn, routing::get, Router};
use futures::stream;
use opentelemetry::trace::{SpanId, SpanKind, Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor, SpanData};
use std::convert::Infallible;
use std::time::Duration;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const THING_ID: &str = "7b0f3c1e-5a52-4d0e-9a39-3f1f4f2f9c11";

fn app() -> Router {
    Router::new()
        .route(
            "/v1/things/{id}",
            get(|| async {
                let chunks = stream::unfold(0u8, |n| async move {
                    if n >= 3 {
                        return None;
                    }
                    if n == 2 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Some((Ok::<_, Infallible>(format!("chunk{n}")), n + 1))
                });
                Body::from_stream(chunks)
            }),
        )
        .route(
            "/v1/fail",
            get(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "customer text",
                )
            }),
        )
        .layer(from_fn(request_correlation_middleware))
}

struct Harness {
    exporter: InMemorySpanExporter,
    _guard: tracing::subscriber::DefaultGuard,
}

fn harness() -> Harness {
    let exporter = InMemorySpanExporter::default();
    let provider = tracer_provider_with_processor(1.0, SimpleSpanProcessor::new(exporter.clone()));
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    Harness {
        exporter,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

fn request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

fn assert_only_allowlisted(span: &SpanData) {
    for kv in &span.attributes {
        assert!(
            SPAN_ATTRIBUTE_ALLOWLIST.contains(&kv.key.as_str()),
            "attribute {} is not allowlisted",
            kv.key
        );
    }
    assert!(span.events.is_empty());
    assert!(span.links.is_empty());
}

#[tokio::test]
async fn streamed_response_exports_one_server_span_after_the_body_finishes() {
    let harness = harness();
    let uri = format!("/v1/things/{THING_ID}");

    let response = app().oneshot(request(&uri)).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        harness.exporter.get_finished_spans().unwrap().is_empty(),
        "span must stay open after the headers are returned"
    );

    let mut body = response.into_body();
    let first = http_body_util::BodyExt::frame(&mut body)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "chunk0");
    assert!(
        harness.exporter.get_finished_spans().unwrap().is_empty(),
        "span must stay open while the body is streaming"
    );

    let rest = http_body_util::BodyExt::collect(body)
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(rest, "chunk1chunk2");
    let drained_at = std::time::SystemTime::now();

    let spans = harness.exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "GET /v1/things/{id}");
    assert_eq!(span.span_kind, SpanKind::Server);
    assert_eq!(
        attribute(span, "http.route").as_deref(),
        Some("/v1/things/{id}")
    );
    assert_eq!(
        attribute(span, "http.request.method").as_deref(),
        Some("GET")
    );
    assert_eq!(
        attribute(span, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert!(span.end_time <= drained_at);
    assert!(
        span.end_time.duration_since(span.start_time).unwrap() >= Duration::from_millis(50),
        "span must cover the delayed final chunk"
    );
    assert_only_allowlisted(span);
    assert!(attribute(span, "request_id").is_some());
    let exported = format!("{:?} {:?}", span.name, span.attributes);
    assert!(!exported.contains(THING_ID), "path id leaked: {exported}");
    assert!(matches!(span.status, Status::Unset));
}

#[tokio::test]
async fn server_error_exports_error_status_without_a_description() {
    let harness = harness();

    let response = app().oneshot(request("/v1/fail")).await.unwrap();
    assert_eq!(response.status(), 500);
    let _ = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap();

    let spans = harness.exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(
        span.status,
        Status::Error {
            description: "".into()
        }
    );
    assert_eq!(
        attribute(span, "http.response.status_code").as_deref(),
        Some("500")
    );
    assert_only_allowlisted(span);
    assert!(!format!("{span:?}").contains("customer text"));
}

#[tokio::test]
async fn unmatched_route_is_named_without_the_raw_path() {
    let harness = harness();

    let response = app()
        .oneshot(request(&format!("/other/{THING_ID}")))
        .await
        .unwrap();
    let _ = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap();

    let spans = harness.exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "GET unmatched");
    assert!(!format!("{:?}", spans[0]).contains(THING_ID));
}

#[tokio::test]
async fn inbound_traceparent_does_not_choose_the_trace() {
    let harness = harness();
    let inbound_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let mut inbound = request(&format!("/v1/things/{THING_ID}"));
    inbound.headers_mut().insert(
        "traceparent",
        format!("00-{inbound_trace}-00f067aa0ba902b7-01")
            .parse()
            .unwrap(),
    );
    inbound
        .headers_mut()
        .insert("tracestate", "vendor=value".parse().unwrap());

    let response = app().oneshot(inbound).await.unwrap();
    let _ = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap();

    let spans = harness.exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_ne!(spans[0].span_context.trace_id().to_string(), inbound_trace);
    assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
    assert!(!spans[0].parent_span_is_remote);
}
