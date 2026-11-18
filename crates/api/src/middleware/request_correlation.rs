use axum::{
    body::{Body, Bytes},
    extract::MatchedPath,
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::Instrument;
use uuid::Uuid;

/// Response body that keeps the request span open until the body is fully
/// sent or dropped, and enters the span while the body is polled, so work
/// driven by the body (a streamed completion, usage recording) runs inside
/// the request span.
struct SpanBody {
    inner: Body,
    span: tracing::Span,
}

impl Drop for SpanBody {
    fn drop(&mut self) {
        // Drop the wrapped body inside the span, so work its destructor starts
        // (a streamed completion recording its usage) is parented to it.
        let _entered = self.span.enter();
        self.inner = Body::empty();
    }
}

impl http_body::Body for SpanBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        let _entered = this.span.enter();
        Pin::new(&mut this.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod privacy_log_scanner;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Clone, Copy)]
pub struct RequestCorrelation {
    /// Chosen by the server for every request: the id in our logs and the one
    /// sent to providers and cloud-platform.
    pub request_id: Uuid,
}

pub async fn request_correlation_middleware(mut request: Request<Body>, next: Next) -> Response {
    // A valid inbound id is echoed in the response and logged next to ours as
    // `client_request_id`; look a customer-reported id up by that field.
    let client_request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok());
    let request_id = Uuid::new_v4();

    request
        .extensions_mut()
        .insert(RequestCorrelation { request_id });

    let method = request.method().clone();
    let path = log_safe_path(request.uri().path());
    // The route template, never the raw path: path segments can carry ids.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let otel_method = super::metrics::bounded_method(&method);
    let otel_name = format!("{otel_method} {route}");
    // Every request starts its own trace: an inbound `traceparent` is never
    // read, and the span has no parent.
    let span = tracing::info_span!(
        parent: None,
        "http_request",
        otel.name = %otel_name,
        otel.kind = "server",
        http.request.method = otel_method,
        http.route = %route,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        request_id = %request_id,
        client_request_id = client_request_id.map(tracing::field::display),
        method = %method,
        path = %path,
        // Filled in by API-key authentication, so every line logged while
        // serving the request names its tenant.
        org_id = tracing::field::Empty,
        workspace_id = tracing::field::Empty,
        api_key_id = tracing::field::Empty,
    );

    // Calls to cloud-platform made while serving this request carry its id.
    // Only when some cloud-platform area is on: with every flag off the request
    // carries no extra scope.
    let run = async move { next.run(request).await };
    let mut response = if database::cp_client::client::configured() {
        database::cp_client::with_request_id(request_id, run)
            .instrument(span.clone())
            .await
    } else {
        run.instrument(span.clone()).await
    };
    let status = response.status();
    span.record("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
    }
    let trace_id = span.in_scope(inference_providers::tracing_context::current_trace_id);
    let response_request_id = client_request_id.unwrap_or(request_id);
    if let Ok(value) = HeaderValue::from_str(&response_request_id.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
        prevent_request_id_caching(response.headers_mut());
    }
    tracing::debug!(
        request_id = %request_id,
        client_request_id = client_request_id.map(tracing::field::display),
        method = %method,
        path = %path,
        status = status.as_u16(),
        trace_id = trace_id.as_deref(),
        "request completed"
    );
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(SpanBody { inner: body, span }))
}

fn prevent_request_id_caching(headers: &mut HeaderMap) {
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

pub(crate) fn log_safe_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if should_redact_path_segment(segment) {
                "[redacted]"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn should_redact_path_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }

    Uuid::parse_str(segment).is_ok() || is_long_token_segment(segment)
}

fn is_long_token_segment(segment: &str) -> bool {
    segment.len() >= 32
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::privacy_log_scanner::assert_production_logs_exclude_forbidden_expressions;
    use super::*;
    use axum::{middleware::from_fn, routing::post, Router};
    use std::io;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use tracing::Level;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedLogsWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturedLogsWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut logs = self
                .0
                .lock()
                .expect("captured logs mutex should not poison");
            logs.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn writer(&self) -> CapturedLogsWriter {
            CapturedLogsWriter(Arc::clone(&self.0))
        }

        fn contents(&self) -> String {
            let logs = self
                .0
                .lock()
                .expect("captured logs mutex should not poison");
            String::from_utf8_lossy(&logs).into_owned()
        }
    }

    async fn served_ids(inbound: Option<&str>) -> (String, Uuid) {
        let app = Router::new()
            .route(
                "/id",
                post(
                    |axum::Extension(correlation): axum::Extension<RequestCorrelation>| async move {
                        correlation.request_id.to_string()
                    },
                ),
            )
            .layer(from_fn(request_correlation_middleware));
        let mut builder = Request::builder().method("POST").uri("/id");
        if let Some(inbound) = inbound {
            builder = builder.header(REQUEST_ID_HEADER, inbound);
        }
        let response = app
            .oneshot(
                builder
                    .body(Body::empty())
                    .expect("test request should build"),
            )
            .await
            .expect("test request should complete");
        let echoed = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("response carries x-request-id")
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let internal = std::str::from_utf8(&body)
            .ok()
            .and_then(|id| Uuid::parse_str(id).ok())
            .expect("handler returns its request id");
        (echoed, internal)
    }

    #[tokio::test]
    async fn inbound_request_id_is_echoed_and_the_server_id_is_used_inside() {
        let inbound = Uuid::new_v4().to_string();
        let (echoed, internal) = served_ids(Some(&inbound)).await;
        assert_eq!(echoed, inbound);
        assert_ne!(internal.to_string(), inbound);

        let (echoed, internal) = served_ids(None).await;
        assert_eq!(echoed, internal.to_string());

        let (echoed, internal) = served_ids(Some("not-a-uuid")).await;
        assert_eq!(echoed, internal.to_string());
    }

    #[tokio::test]
    async fn tracing_logs_exclude_customer_content() {
        assert_production_logs_exclude_forbidden_expressions();

        let logs = CapturedLogs::default();
        let writer_logs = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer_logs.writer())
            .finish();
        let request_id = Uuid::new_v4();
        let body = r#"{"prompt":"CUSTOMER_PROMPT_SENTINEL","tool_arguments":{"secret":"TOOL_ARG_SENTINEL"},"api_key":"sk-log-sentinel"}"#;
        let body_hash = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(body.as_bytes()))
        };
        let token_path = "tok_live_customer_invitation_secret_1234567890";
        let app = Router::new()
            .route("/v1/invitations/{token}", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                crate::middleware::body_hash::BodyHashLimit(1024),
                crate::middleware::body_hash::body_hash_middleware,
            ))
            .layer(from_fn(request_correlation_middleware));

        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/invitations/{token_path}"))
                    .header(REQUEST_ID_HEADER, request_id.to_string())
                    .header("x-org-id", "spoofed-org-sentinel")
                    .header("authorization", "Bearer sk-log-sentinel")
                    .header("User-Agent", "USER_AGENT_LOG_SENTINEL")
                    .body(Body::from(body))
                    .expect("test request should build"),
            )
            .await
            .expect("test request should complete");

        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .expect("request-specific responses should disable shared caching"),
            "no-store"
        );
        let captured = logs.contents();
        assert!(captured.contains(&request_id.to_string()));
        assert!(captured.contains("method=POST"));
        assert!(captured.contains("path=/v1/invitations/[redacted]"));
        assert!(captured.contains("status=200"));
        for forbidden in [
            "CUSTOMER_PROMPT_SENTINEL",
            "TOOL_ARG_SENTINEL",
            "sk-log-sentinel",
            "spoofed-org-sentinel",
            "USER_AGENT_LOG_SENTINEL",
            token_path,
            &body_hash,
        ] {
            assert!(
                !captured.contains(forbidden),
                "captured logs must not contain {forbidden}; logs: {captured}"
            );
        }
    }
}
