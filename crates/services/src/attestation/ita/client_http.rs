use std::{error::Error as _, net::IpAddr};

use bytes::Bytes;
use reqwest::{
    header::{HeaderMap, RETRY_AFTER},
    StatusCode, Url,
};
use serde::de::DeserializeOwned;

use super::ItaClientError;

pub(super) async fn parse_json_response<T>(
    response: reqwest::Response,
    body_limit: usize,
) -> Result<T, ItaClientError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status, response.headers()));
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

fn status_error(status: StatusCode, headers: &HeaderMap) -> ItaClientError {
    match status {
        StatusCode::TOO_MANY_REQUESTS => ItaClientError::RateLimited {
            retry_after: response_header(headers, RETRY_AFTER.as_str()),
        },
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
            ItaClientError::TransientStatus { status }
        }
        _ => ItaClientError::NonRetryableStatus { status },
    }
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
