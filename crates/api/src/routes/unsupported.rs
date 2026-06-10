use crate::models::ErrorResponse;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::{get, post};
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
        .route(
            "/batches",
            get(openai_endpoint_not_implemented).post(openai_endpoint_not_implemented),
        )
        .route(
            "/threads",
            get(openai_endpoint_not_implemented).post(openai_endpoint_not_implemented),
        )
        .route("/assistants", get(openai_endpoint_not_implemented))
        .route("/responses", get(openai_endpoint_not_implemented))
        .route("/models", post(openai_endpoint_not_implemented))
        .route(
            "/models/{*model_id}",
            get(openai_endpoint_not_implemented).delete(openai_endpoint_not_implemented),
        )
}

pub async fn openai_endpoint_not_implemented(
    request: Request,
) -> (StatusCode, Json<ErrorResponse>) {
    let message = format!(
        "{} {} is not implemented by NEAR AI Cloud yet",
        request.method(),
        request.uri().path()
    );

    (
        StatusCode::NOT_IMPLEMENTED,
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
            (Method::POST, "/images/variations"),
            (Method::POST, "/audio/translations"),
            (Method::POST, "/moderations"),
            (Method::GET, "/batches"),
            (Method::POST, "/batches"),
            (Method::GET, "/threads"),
            (Method::POST, "/threads"),
            (Method::GET, "/assistants"),
            (Method::GET, "/responses"),
            (Method::POST, "/models"),
            (Method::GET, "/models/openai/gpt-oss-120b"),
            (Method::DELETE, "/models/openai/gpt-oss-120b"),
        ];

        for (method, path) in cases {
            let app = openai_compat_routes();
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
