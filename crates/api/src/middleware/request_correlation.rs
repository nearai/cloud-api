use axum::{
    body::Body,
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;
use uuid::Uuid;

#[cfg(test)]
mod privacy_log_scanner;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Clone, Copy)]
pub struct RequestCorrelation {
    pub request_id: Uuid,
}

pub async fn request_correlation_middleware(mut request: Request<Body>, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4);

    request
        .extensions_mut()
        .insert(RequestCorrelation { request_id });

    let method = request.method().clone();
    let path = log_safe_path(request.uri().path());
    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );

    let mut response = async move { next.run(request).await }
        .instrument(span)
        .await;
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
        prevent_request_id_caching(response.headers_mut());
    }
    tracing::debug!(
        request_id = %request_id,
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        "request completed"
    );
    response
}

fn prevent_request_id_caching(headers: &mut HeaderMap) {
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn log_safe_path(path: &str) -> String {
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
        let body = Body::from(
            r#"{"prompt":"CUSTOMER_PROMPT_SENTINEL","tool_arguments":{"secret":"TOOL_ARG_SENTINEL"},"api_key":"sk-log-sentinel"}"#,
        );
        let token_path = "tok_live_customer_invitation_secret_1234567890";
        let app = Router::new()
            .route("/v1/invitations/{token}", post(|| async { "ok" }))
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
                    .body(body)
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
        ] {
            assert!(
                !captured.contains(forbidden),
                "captured logs must not contain {forbidden}; logs: {captured}"
            );
        }
    }
}
