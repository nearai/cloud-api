use crate::models::ErrorResponse;
use crate::routes::common::{HEADER_SHOULD_RETRY, SHOULD_RETRY_FALSE};
use axum::extract::OriginalUri;
use axum::http::{Method, StatusCode};
use axum::routing::{any, get, post};
use axum::{Json, Router};

/// Known OpenAI-compatible endpoints that are not implemented yet.
///
/// Registering these routes keeps clients from receiving Axum's empty 404/405
/// fallback for recognized API surfaces while preserving a clear not-implemented
/// contract until the endpoints get real handlers.
pub fn openai_compat_routes() -> Router {
    Router::new()
        .route("/images/variations", post(openai_endpoint_not_implemented))
        .route("/audio/translations", post(openai_endpoint_not_implemented))
        .route("/moderations", post(openai_endpoint_not_implemented))
        .route("/batches", any(openai_endpoint_not_implemented))
        .route("/batches/{*path}", any(openai_endpoint_not_implemented))
        .route("/threads", any(openai_endpoint_not_implemented))
        .route("/threads/{*path}", any(openai_endpoint_not_implemented))
        .route("/assistants", any(openai_endpoint_not_implemented))
        .route("/assistants/{*path}", any(openai_endpoint_not_implemented))
        .route("/responses", get(openai_endpoint_not_implemented))
        .route("/models", post(openai_endpoint_not_implemented))
        .route("/models/{*model_id}", any(openai_endpoint_not_implemented))
}

/// Global router fallback for requests that match no route.
///
/// Returns a stable generic OpenAI-style error envelope instead of Axum's
/// default empty-body 404, so unmapped paths never surface framework or
/// infrastructure detail (nearai/infra#192). The body is intentionally
/// static: no method/path echo, no version, no implementation hints.
pub async fn unknown_route() -> (
    StatusCode,
    [(&'static str, &'static str); 1],
    Json<ErrorResponse>,
) {
    (
        StatusCode::NOT_FOUND,
        [(HEADER_SHOULD_RETRY, SHOULD_RETRY_FALSE)],
        Json(ErrorResponse::new(
            "Not found".to_string(),
            "invalid_request_error".to_string(),
        )),
    )
}

/// Fallback for requests that match a route path but not the HTTP method:
/// the same stable generic envelope as [`unknown_route`], with 405.
pub async fn method_not_allowed() -> (
    StatusCode,
    [(&'static str, &'static str); 1],
    Json<ErrorResponse>,
) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(HEADER_SHOULD_RETRY, SHOULD_RETRY_FALSE)],
        Json(ErrorResponse::new(
            "Method not allowed".to_string(),
            "invalid_request_error".to_string(),
        )),
    )
}

pub async fn openai_endpoint_not_implemented(
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> (
    StatusCode,
    [(&'static str, &'static str); 1],
    Json<ErrorResponse>,
) {
    let message = format!(
        "{} {} is not implemented by NEAR AI Cloud yet",
        method,
        uri.path()
    );

    (
        StatusCode::NOT_IMPLEMENTED,
        [(HEADER_SHOULD_RETRY, SHOULD_RETRY_FALSE)],
        Json(ErrorResponse::new(message, "not_implemented".to_string())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header::CONTENT_TYPE, Method, Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn unsupported_openai_routes_return_json_envelope() {
        let cases = [
            (Method::POST, "/v1/images/variations"),
            (Method::POST, "/v1/audio/translations"),
            (Method::POST, "/v1/moderations"),
            (Method::GET, "/v1/batches"),
            (Method::POST, "/v1/batches"),
            (Method::GET, "/v1/batches/batch_123"),
            (Method::POST, "/v1/batches/batch_123/cancel"),
            (Method::GET, "/v1/threads"),
            (Method::POST, "/v1/threads"),
            (Method::GET, "/v1/threads/thread_123"),
            (Method::DELETE, "/v1/threads/thread_123"),
            (Method::GET, "/v1/assistants"),
            (Method::POST, "/v1/assistants"),
            (Method::GET, "/v1/assistants/asst_123"),
            (Method::GET, "/v1/responses"),
            (Method::POST, "/v1/models"),
            (Method::GET, "/v1/models/openai/gpt-oss-120b"),
            (Method::DELETE, "/v1/models/openai/gpt-oss-120b"),
        ];

        for (method, path) in cases {
            let app = Router::new().nest("/v1", openai_compat_routes());
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .method(method.clone())
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::NOT_IMPLEMENTED,
                "{method} {path}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .map(|value| value.to_str().unwrap()),
                Some("application/json"),
                "{method} {path}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(HEADER_SHOULD_RETRY)
                    .map(|value| value.to_str().unwrap()),
                Some(SHOULD_RETRY_FALSE),
                "{method} {path}"
            );

            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(body["error"]["type"], "not_implemented", "{method} {path}");
            assert_eq!(body["error"]["param"], serde_json::Value::Null);
            assert_eq!(body["error"]["code"], serde_json::Value::Null);

            let message = body["error"]["message"].as_str().unwrap();
            assert!(
                message.contains(method.as_str()) && message.contains(path),
                "message should mention {method} {path}, got {message:?}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_route_fallback_returns_generic_json_envelope() {
        async fn ok() -> &'static str {
            "ok"
        }

        for (method, path) in [
            (Method::GET, "/nonexistent"),
            (Method::GET, "/test.html"),
            (Method::POST, "/v1/nonexistent"),
            (Method::DELETE, "/.env"),
        ] {
            let app: Router = Router::new()
                .route("/v1/health", get(ok))
                .fallback(unknown_route);
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .method(method.clone())
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .map(|value| value.to_str().unwrap()),
                Some("application/json"),
                "{method} {path}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(HEADER_SHOULD_RETRY)
                    .map(|value| value.to_str().unwrap()),
                Some(SHOULD_RETRY_FALSE),
                "{method} {path}"
            );

            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

            // The body must stay static and generic: no path echo, no
            // implementation detail.
            assert_eq!(body["error"]["message"], "Not found", "{method} {path}");
            assert_eq!(
                body["error"]["type"], "invalid_request_error",
                "{method} {path}"
            );
            assert_eq!(body["error"]["param"], serde_json::Value::Null);
            assert_eq!(body["error"]["code"], serde_json::Value::Null);
        }
    }

    #[tokio::test]
    async fn method_not_allowed_fallback_returns_generic_json_envelope() {
        async fn ok() -> &'static str {
            "ok"
        }

        let app: Router = Router::new()
            .route("/only-get", get(ok))
            .fallback(unknown_route)
            .method_not_allowed_fallback(method_not_allowed);
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/only-get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some("application/json")
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["message"], "Method not allowed");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn unsupported_routes_merge_with_existing_method_routes() {
        async fn ok() -> &'static str {
            "ok"
        }

        let _router: Router = Router::new()
            .route("/responses", post(ok))
            .route("/models", get(ok))
            .merge(openai_compat_routes());
    }
}
