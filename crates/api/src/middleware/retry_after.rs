// Retry-After on 429 responses
//
// Guarantees every client-facing HTTP 429 carries a machine-readable
// `Retry-After` header. Standard OpenAI/Anthropic SDK backoff honors
// `Retry-After`; without it clients only get retry guidance as prose in the
// JSON error body ("Please retry with exponential backoff") and fall back to
// their own schedule.
//
// Sites that know a better value set the header themselves and win over this
// default:
// - the per-API-key fixed-window limiter sets the window length (60s), see
//   `rate_limit.rs`;
// - the ITA attestation path propagates the upstream `Retry-After` when Intel
//   Trust Authority supplies one, see `routes/attestation/errors.rs`.
//
// Everything else — per-(org,model) concurrency-cap 429s, upstream provider
// 429 passthrough, and `ServiceOverloaded` ("all backends exhausted") — is a
// transient condition where a short retry hint is appropriate, so this layer
// fills in `DEFAULT_RETRY_AFTER_SECS`. Upstream provider 429s cannot carry the
// provider's own `Retry-After` today: `CompletionError::HttpError` only keeps
// the status code and message, and the provider pool retries with its own
// backoff ladder before surfacing the error, so any captured value would be
// stale by the time it reached the client.

use axum::{
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    response::Response,
};

/// Default `Retry-After` seconds for 429 responses that did not set their own
/// value. A short seed: the error body already tells clients to back off
/// exponentially, and the conditions behind these 429s (concurrency caps,
/// transient overload) usually clear quickly.
const DEFAULT_RETRY_AFTER_SECS: u64 = 2;

/// `map_response` layer: add a default `Retry-After` header to any 429 that
/// does not already carry one. Never overrides a value set closer to the
/// source (per-key limiter window, upstream ITA propagation).
pub async fn retry_after_middleware(mut response: Response) -> Response {
    if response.status() == StatusCode::TOO_MANY_REQUESTS
        && !response.headers().contains_key(RETRY_AFTER)
    {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(DEFAULT_RETRY_AFTER_SECS));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn response_with_status(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn adds_default_retry_after_to_429_without_header() {
        let response =
            retry_after_middleware(response_with_status(StatusCode::TOO_MANY_REQUESTS)).await;

        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        assert_eq!(retry_after, Some(DEFAULT_RETRY_AFTER_SECS));
    }

    #[tokio::test]
    async fn preserves_existing_retry_after_on_429() {
        // e.g. the per-key limiter (60s window) or an upstream-propagated
        // value (ITA) must not be clobbered by the default.
        let mut response = response_with_status(StatusCode::TOO_MANY_REQUESTS);
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static("60"));

        let response = retry_after_middleware(response).await;

        assert_eq!(
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("60")
        );
    }

    #[tokio::test]
    async fn leaves_non_429_responses_untouched() {
        for status in [
            StatusCode::OK,
            StatusCode::BAD_REQUEST,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let response = retry_after_middleware(response_with_status(status)).await;
            assert!(
                response.headers().get(RETRY_AFTER).is_none(),
                "{status} must not get a Retry-After header"
            );
        }
    }
}
