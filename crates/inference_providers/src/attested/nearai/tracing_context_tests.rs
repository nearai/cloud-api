//! The authenticated request's ids reach the self-hosted backend on inference
//! calls, and never on control calls made while a request is in scope.

use super::{Config, Provider};
use crate::tracing_context::{with_tracing_context, TracingContext};
use crate::{ImageEditParams, InferenceProvider, SystemOneRequest};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use wiremock::{matchers::any, Mock, MockServer, ResponseTemplate};

const TRACING_HEADERS: [&str; 3] = ["x-request-id", "x-org-id", "x-workspace-id"];

fn context() -> TracingContext {
    TracingContext {
        request_id: uuid::Uuid::new_v4(),
        org_id: uuid::Uuid::new_v4(),
        workspace_id: uuid::Uuid::new_v4(),
    }
}

async fn mock_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    server
}

fn provider(server: &MockServer) -> Provider {
    Provider::new(Config {
        base_url: server.uri(),
        api_key: None,
        completion_timeout_seconds: 5,
        control_timeout_seconds: 5,
    })
}

fn header(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Every request the server saw carries exactly the context's ids.
async fn assert_ids_sent(server: &MockServer, context: TracingContext, operation: &str) {
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.is_empty(),
        "{operation}: no request reached the backend"
    );
    for request in &requests {
        assert_eq!(
            header(request, "x-request-id"),
            Some(context.request_id.to_string()),
            "{operation}: X-Request-Id"
        );
        assert_eq!(
            header(request, "x-org-id"),
            Some(context.org_id.to_string()),
            "{operation}: X-Org-Id"
        );
        assert_eq!(
            header(request, "x-workspace-id"),
            Some(context.workspace_id.to_string()),
            "{operation}: X-Workspace-Id"
        );
    }
}

async fn assert_no_ids_sent(server: &MockServer, operation: &str) {
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.is_empty(),
        "{operation}: no request reached the backend"
    );
    for request in &requests {
        for name in TRACING_HEADERS {
            assert_eq!(
                header(request, name),
                None,
                "{operation}: {name} must not be sent on a control call"
            );
        }
    }
}

#[tokio::test]
async fn image_edit_sends_the_ids_in_scope() {
    let server = mock_server().await;
    let provider = provider(&server);
    let context = context();
    let params = Arc::new(ImageEditParams {
        model: "test-model".into(),
        prompt: "synthetic".into(),
        image: Arc::new(vec![0x89, b'P', b'N', b'G']),
        size: None,
        response_format: None,
    });

    let _ = with_tracing_context(context, provider.image_edit(params, "hash".into())).await;

    assert_ids_sent(&server, context, "image_edit").await;
}

#[tokio::test]
async fn privacy_classify_sends_the_ids_in_scope_and_drops_body_keys() {
    let server = mock_server().await;
    let provider = provider(&server);
    let context = context();
    let extra = HashMap::from([
        ("x_org_id".to_string(), json!("client-value")),
        ("x_request_id".to_string(), json!("client-value")),
    ]);

    let _ = with_tracing_context(
        context,
        provider.privacy_classify_raw(bytes::Bytes::from_static(b"{}"), extra),
    )
    .await;

    assert_ids_sent(&server, context, "privacy_classify").await;
    for request in server.received_requests().await.unwrap() {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        assert!(
            !body.contains("client-value"),
            "body keys must never be sent"
        );
    }
}

#[tokio::test]
async fn signature_fetch_sends_the_ids_in_scope() {
    let server = mock_server().await;
    let provider = provider(&server);
    let context = context();

    let _ = with_tracing_context(
        context,
        provider.get_signature("chat-1", Some("ecdsa".into())),
    )
    .await;

    assert_ids_sent(&server, context, "get_signature").await;
}

#[tokio::test]
async fn control_calls_in_scope_send_no_ids() {
    let context = context();

    let server = mock_server().await;
    let _ = with_tracing_context(context, provider(&server).models()).await;
    assert_no_ids_sent(&server, "models").await;

    let server = mock_server().await;
    let _ = with_tracing_context(
        context,
        provider(&server).get_attestation_report("test-model".into(), None, None, None, false),
    )
    .await;
    assert_no_ids_sent(&server, "get_attestation_report").await;
}

#[tokio::test]
async fn systemone_in_scope_sends_no_tenant_ids() {
    let server = mock_server().await;
    let provider = provider(&server);
    let request: SystemOneRequest = serde_json::from_value(json!({
        "model": "test-model", "state": "x",
        "questions": {"q": {"type": "noul", "instructions": "synthetic"}}
    }))
    .expect("synthetic System One request");

    let _ = with_tracing_context(context(), provider.systemone(request, "hash".into(), None)).await;

    assert_no_ids_sent(&server, "systemone").await;
}

mod traceparent {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::layer::SubscriberExt;

    fn is_well_formed(value: &str) -> bool {
        let parts: Vec<&str> = value.split('-').collect();
        parts.len() == 4
            && parts[0] == "00"
            && parts[1].len() == 32
            && parts[2].len() == 16
            && parts[3].len() == 2
            && parts[1..]
                .iter()
                .all(|part| part.chars().all(|c| c.is_ascii_hexdigit()))
    }

    #[tokio::test]
    async fn sent_on_inference_calls_under_an_otel_layer() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let tracer_provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("test")));
        let _guard = tracing::subscriber::set_default(subscriber);

        let server = mock_server().await;
        let provider = provider(&server);
        let span = tracing::info_span!("outer");
        let trace_id = {
            let _entered = span.enter();
            crate::tracing_context::current_trace_id().expect("valid trace id in span")
        };
        assert_eq!(trace_id.len(), 32);
        let params = Arc::new(ImageEditParams {
            model: "test-model".into(),
            prompt: "synthetic".into(),
            image: Arc::new(vec![0x89, b'P', b'N', b'G']),
            size: None,
            response_format: None,
        });
        let call = provider.image_edit(params, "hash".into());
        let _ = tracing::Instrument::instrument(call, span).await;

        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        for request in &requests {
            let value = header(request, "traceparent").expect("traceparent sent");
            assert!(is_well_formed(&value), "malformed traceparent: {value}");
            assert!(value.contains(&trace_id));
            assert_eq!(header(request, "tracestate"), None);
        }
    }

    #[tokio::test]
    async fn absent_without_an_otel_layer() {
        let server = mock_server().await;
        let provider = provider(&server);
        let params = Arc::new(ImageEditParams {
            model: "test-model".into(),
            prompt: "synthetic".into(),
            image: Arc::new(vec![0x89, b'P', b'N', b'G']),
            size: None,
            response_format: None,
        });
        let _ = provider.image_edit(params, "hash".into()).await;

        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        for request in &requests {
            assert_eq!(header(request, "traceparent"), None);
            assert_eq!(header(request, "tracestate"), None);
        }
        assert_eq!(crate::tracing_context::current_trace_id(), None);
    }
}
