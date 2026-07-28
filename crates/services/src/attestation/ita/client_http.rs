use std::{error::Error as _, net::IpAddr};

use bytes::Bytes;
use reqwest::{
    header::{HeaderMap, RETRY_AFTER},
    StatusCode, Url,
};
use serde::de::DeserializeOwned;

use super::ItaClientError;

const ERROR_DETAIL_BODY_LIMIT: usize = 4 * 1024;
const ERROR_DETAIL_MAX_CHARS: usize = 256;

pub(super) async fn parse_json_response<T>(
    response: reqwest::Response,
    body_limit: usize,
) -> Result<T, ItaClientError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        // Classify by status line before touching the body: transient and
        // rate-limit responses discard diagnostic detail, and awaiting a
        // slow error body here would burn the read timeout once per retry
        // attempt before with_retry can fire the next one.
        return Err(match status {
            StatusCode::TOO_MANY_REQUESTS => ItaClientError::RateLimited {
                retry_after: response_header(response.headers(), RETRY_AFTER.as_str()),
            },
            StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT => ItaClientError::TransientStatus { status },
            _ => ItaClientError::NonRetryableStatus {
                status,
                detail: read_error_detail(response).await,
            },
        });
    }

    let body = read_limited_body(response, body_limit).await?;
    serde_json::from_slice(&body).map_err(|_| ItaClientError::UpstreamResponse {
        reason: "malformed JSON",
    })
}

pub(super) fn response_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

pub(super) fn transport_error(source: reqwest::Error) -> ItaClientError {
    if source.is_timeout() {
        return ItaClientError::Timeout;
    }
    let retryable = is_retryable_transport_error(&source);
    ItaClientError::Transport { retryable, source }
}

pub(super) fn is_connection_reset(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("connection reset")
}

pub(super) fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

async fn read_limited_body(
    mut response: reqwest::Response,
    body_limit: usize,
) -> Result<Bytes, ItaClientError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let Some(next_len) = body.len().checked_add(chunk.len()) else {
            return Err(ItaClientError::UpstreamResponse {
                reason: "oversized body",
            });
        };
        if next_len > body_limit {
            return Err(ItaClientError::UpstreamResponse {
                reason: "oversized body",
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// Best-effort extraction of ITA's error body for diagnostics. ITA error
/// bodies describe attestation infrastructure failures (never customer
/// data); the text is bounded and control characters are stripped.
/// Oversized bodies are truncated rather than discarded — long appraisal
/// failures are exactly where the detail matters — and a mid-read
/// transport error keeps whatever arrived.
async fn read_error_detail(mut response: reqwest::Response) -> Option<String> {
    let mut body = Vec::new();
    while body.len() < ERROR_DETAIL_BODY_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = (ERROR_DETAIL_BODY_LIMIT - body.len()).min(chunk.len());
                body.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    extract_error_detail(&body)
}

fn extract_error_detail(body: &[u8]) -> Option<String> {
    let text = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => {
            let mut json_detail = None;
            for key in ["error", "message", "detail"] {
                if let Some(text) = value.get(key).and_then(serde_json::Value::as_str) {
                    json_detail = Some(text.to_string());
                    break;
                }
            }
            json_detail.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
        }
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    };
    let sanitized: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(ERROR_DETAIL_MAX_CHARS)
        .collect();
    let trimmed = sanitized.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    let mut source = error.source();
    while let Some(error_source) = source {
        if is_connection_reset(error_source.to_string().as_str()) {
            return true;
        }
        source = error_source.source();
    }
    false
}
