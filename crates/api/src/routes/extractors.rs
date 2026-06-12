//! Custom request extractors that normalize deserialization-layer failures
//! into the OpenAI-compatible error envelope.
//!
//! Background (issue #781 L2): axum's built-in `Json<T>` extractor rejects a
//! request *before* the handler body runs whenever the body is malformed JSON,
//! is missing a required field, or has a field of the wrong type. The default
//! `JsonRejection::into_response()` emits a **bare string** body (e.g.
//! `"Failed to deserialize the JSON body into the target type: ..."`) with a
//! `text/plain` content type, and uses `422 Unprocessable Entity` for
//! shape/type errors. That is a different error *shape* than every
//! business-validated error this API returns, which is the OpenAI envelope:
//!
//! ```json
//! { "error": { "message": "...", "type": "invalid_request_error",
//!              "param": null, "code": null } }
//! ```
//!
//! Two incompatible error shapes force clients to special-case our gateway.
//! `OpenAiJson<T>` is a drop-in replacement for `axum::Json<T>` on inference
//! handlers: on success it behaves identically (deserializes into `T`); on a
//! deserialization failure it wraps the rejection into the OpenAI envelope and
//! prefers `400 Bad Request` for malformed/invalid bodies (matching OpenAI),
//! instead of axum's `422`.
//!
//! Only the error path differs from `axum::Json`; valid requests are untouched.

use crate::models::ErrorResponse;
use axum::{
    extract::{rejection::JsonRejection, FromRequest, Json, Request},
    http::StatusCode,
    response::{IntoResponse, Json as ResponseJson, Response},
};

/// Error type used in the OpenAI envelope for request-deserialization
/// failures. Matches the `type` produced by the business-validation path
/// (`ChatCompletionRequest::validate_request` etc.) so clients see a single,
/// consistent error shape regardless of whether the body failed at the
/// deserialization layer or at business validation.
const INVALID_REQUEST_ERROR: &str = "invalid_request_error";

/// Drop-in replacement for [`axum::Json`] that returns the OpenAI error
/// envelope (rather than a bare string) when the request body fails to
/// deserialize into `T`.
///
/// Behaviour for *valid* requests is identical to `axum::Json<T>` — this only
/// changes the SHAPE and status of deserialization-layer rejections.
pub struct OpenAiJson<T>(pub T);

impl<T, S> FromRequest<S> for OpenAiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = OpenAiJsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(OpenAiJson(value)),
            Err(rejection) => Err(OpenAiJsonRejection(rejection)),
        }
    }
}

/// Rejection wrapper that renders a [`JsonRejection`] as the OpenAI error
/// envelope. Kept as a distinct type (rather than mapping straight to a
/// `Response`) so the extractor satisfies `FromRequest`'s associated
/// `Rejection: IntoResponse` bound.
pub struct OpenAiJsonRejection(JsonRejection);

impl IntoResponse for OpenAiJsonRejection {
    fn into_response(self) -> Response {
        let rejection = self.0;

        // Prefer 400 for malformed JSON, shape/type errors, and a missing/wrong
        // Content-Type — OpenAI returns 400 for all of these, whereas axum
        // defaults JsonDataError to 422 and MissingJsonContentType to 415. The
        // remaining variant (BytesRejection: body read/too-large) keeps its own
        // status. JsonRejection is #[non_exhaustive], so a catch-all arm is
        // required; only the body SHAPE is normalized for it.
        let status = match &rejection {
            JsonRejection::JsonDataError(_)
            | JsonRejection::JsonSyntaxError(_)
            | JsonRejection::MissingJsonContentType(_) => StatusCode::BAD_REQUEST,
            other => other.status(),
        };

        // `body_text()` carries axum's human-readable detail (including the
        // serde error for syntax/data failures), which is the most useful
        // message we can surface to the client.
        let message = rejection.body_text();
        let body = ErrorResponse::new(message, INVALID_REQUEST_ERROR.to_string());

        (status, ResponseJson(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request},
        routing::post,
        Router,
    };
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Probe {
        #[allow(dead_code)]
        model: String,
        #[allow(dead_code)]
        messages: Vec<String>,
    }

    async fn probe_handler(OpenAiJson(_): OpenAiJson<Probe>) -> StatusCode {
        StatusCode::OK
    }

    fn app() -> Router {
        Router::new().route("/", post(probe_handler))
    }

    async fn post_json(body: &'static str) -> (StatusCode, ErrorResponse) {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: ErrorResponse = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response body should be the OpenAI envelope, not a bare string: {e}; body={}",
                String::from_utf8_lossy(&bytes)
            )
        });
        // Full envelope parity: ErrorResponse::new leaves param/code null.
        assert_eq!(envelope.error.param, None);
        assert_eq!(envelope.error.code, None);
        (status, envelope)
    }

    #[tokio::test]
    async fn malformed_json_is_400_envelope() {
        let (status, envelope) = post_json("{ \"model\": \"x\", ").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(envelope.error.r#type, INVALID_REQUEST_ERROR);
        assert!(!envelope.error.message.is_empty());
    }

    #[tokio::test]
    async fn missing_required_field_is_400_envelope() {
        // Valid JSON, missing `model`/`messages` -> axum would give 422; we map to 400.
        let (status, envelope) = post_json("{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(envelope.error.r#type, INVALID_REQUEST_ERROR);
        assert!(!envelope.error.message.is_empty());
    }

    #[tokio::test]
    async fn wrong_type_field_is_400_envelope() {
        // `messages` is the wrong type (string, not array of strings).
        let (status, envelope) = post_json("{ \"model\": \"x\", \"messages\": \"oops\" }").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(envelope.error.r#type, INVALID_REQUEST_ERROR);
        assert!(!envelope.error.message.is_empty());
    }

    #[tokio::test]
    async fn missing_content_type_is_400_envelope() {
        // No Content-Type header -> axum's MissingJsonContentType (415); we map
        // it to 400 + envelope to match OpenAI.
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::from("{ \"model\": \"x\", \"messages\": [\"hi\"] }"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: ErrorResponse = serde_json::from_slice(&bytes)
            .expect("response body should be the OpenAI envelope, not a bare string");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(envelope.error.r#type, INVALID_REQUEST_ERROR);
        assert!(!envelope.error.message.is_empty());
    }

    #[tokio::test]
    async fn valid_body_passes_through() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{ \"model\": \"x\", \"messages\": [\"hi\"] }"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
