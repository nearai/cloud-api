//! HTTP metrics middleware for tracking request counts and latencies.
//!
//! This middleware records low-cardinality metrics for all HTTP requests:
//! - `cloud_api.http.requests` - Count of HTTP requests by method, endpoint, status
//! - `cloud_api.http.duration` - Histogram of request durations by method, endpoint
//!
//! Endpoints use Axum's matched route templates. Requests that do not match a
//! registered route use a single `/unmatched` bucket.

use axum::{
    body::Body,
    extract::{MatchedPath, State},
    http::{Method, Request},
    middleware::Next,
    response::Response,
};
use services::metrics::{
    consts::{
        get_environment, METRIC_HTTP_DURATION, METRIC_HTTP_REQUESTS, TAG_ENDPOINT, TAG_ENVIRONMENT,
        TAG_METHOD, TAG_STATUS_CODE,
    },
    MetricsServiceTrait,
};
use std::sync::Arc;
use std::time::Instant;

/// State for the metrics middleware
#[derive(Clone)]
pub struct MetricsState {
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
}

fn bounded_method(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => "OTHER",
    }
}

/// Middleware that records HTTP request metrics
pub async fn http_metrics_middleware(
    State(state): State<MetricsState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = bounded_method(req.method());
    let endpoint = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("/unmatched")
        .to_owned();

    let response = next.run(req).await;
    let duration = start.elapsed();
    let status = response.status().as_u16();

    let environment = get_environment();

    let tags = [
        format!("{TAG_METHOD}:{method}"),
        format!("{TAG_ENDPOINT}:{endpoint}"),
        format!("{TAG_STATUS_CODE}:{status}"),
        format!("{TAG_ENVIRONMENT}:{environment}"),
    ];
    let tags_str: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();

    state
        .metrics_service
        .record_latency(METRIC_HTTP_DURATION, duration, &tags_str);
    state
        .metrics_service
        .record_count(METRIC_HTTP_REQUESTS, 1, &tags_str);

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{Method, StatusCode},
        middleware::from_fn_with_state,
        routing::get,
        Router,
    };
    use services::metrics::capturing::CapturingMetricsService;
    use tower::ServiceExt;

    fn metrics_router(metrics: Arc<CapturingMetricsService>) -> Router {
        Router::new()
            .nest(
                "/v1",
                Router::new().route("/signature/{chat_id}", get(|| async { StatusCode::OK })),
            )
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(from_fn_with_state(
                MetricsState {
                    metrics_service: metrics,
                },
                http_metrics_middleware,
            ))
    }

    async fn recorded_tags(method: Method, uri: &str) -> Vec<String> {
        let metrics = Arc::new(CapturingMetricsService::new());
        let response = metrics_router(metrics.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("test request should be valid"),
            )
            .await
            .expect("test router should return a response");

        assert!(matches!(
            response.status(),
            StatusCode::OK | StatusCode::NOT_FOUND
        ));

        metrics
            .get_metrics()
            .into_iter()
            .find(|metric| metric.name == METRIC_HTTP_REQUESTS)
            .map(|metric| metric.tags)
            .expect("request count metric should be recorded")
    }

    fn recorded_tag(tags: &[String], name: &str) -> String {
        let prefix = format!("{name}:");
        tags.iter()
            .find_map(|tag| tag.strip_prefix(&prefix).map(str::to_owned))
            .expect("request count metric should have the requested tag")
    }

    #[tokio::test]
    async fn signature_route_uses_matched_template_instead_of_bare_id() {
        let raw_id = "0123456789abcdef0123456789abcdef";

        let tags = recorded_tags(Method::GET, &format!("/v1/signature/{raw_id}")).await;
        let endpoint = recorded_tag(&tags, TAG_ENDPOINT);

        assert_eq!(endpoint, "/v1/signature/{chat_id}");
        assert!(!endpoint.contains(raw_id));
        assert_eq!(recorded_tag(&tags, TAG_METHOD), "GET");
    }

    #[tokio::test]
    async fn unmatched_scanner_path_uses_single_bounded_bucket() {
        let scanner_path = "/wp-admin/install.php";

        let tags = recorded_tags(Method::GET, scanner_path).await;
        let endpoint = recorded_tag(&tags, TAG_ENDPOINT);

        assert_eq!(endpoint, "/unmatched");
        assert!(!endpoint.contains(scanner_path));
    }

    #[tokio::test]
    async fn extension_scanner_method_uses_single_bounded_bucket() {
        let raw_method = "PROPFIND";
        let method = Method::from_bytes(raw_method.as_bytes())
            .expect("extension method should be valid HTTP syntax");

        let tags = recorded_tags(method, "/wp-admin/install.php").await;
        let recorded_method = recorded_tag(&tags, TAG_METHOD);

        assert_eq!(recorded_method, "OTHER");
        assert!(!recorded_method.contains(raw_method));
    }
}
