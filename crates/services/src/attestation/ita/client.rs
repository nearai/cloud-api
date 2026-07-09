use std::{fmt, time::Duration};

use config::ItaAttestationConfig;
use reqwest::{
    header::{HeaderValue, ACCEPT, CONTENT_TYPE},
    Url,
};

use crate::attestation::ita::{
    ItaAttestRequest, ItaAttestResponse, ItaNonceResponse, ItaVerifierNonce,
};

#[path = "client_error.rs"]
mod error;
#[path = "client_http.rs"]
mod http;
pub use error::ItaClientError;
use http::{is_loopback_url, parse_json_response, response_header, transport_error};

const NONCE_ENDPOINT: &str = "appraisal/v2/nonce";
const ATTEST_ENDPOINT: &str = "appraisal/v2/attest";
const NONCE_BODY_LIMIT: usize = 4 * 1024;
const TOKEN_BODY_LIMIT: usize = 64 * 1024;
const API_KEY_HEADER: &str = "x-api-key";
const REQUEST_ID_HEADER: &str = "Request-Id";
const APPLICATION_JSON: &str = "application/json";

#[derive(Clone)]
pub struct ItaClient {
    http_client: reqwest::Client,
    nonce_url: Url,
    attest_url: Url,
    api_key: HeaderValue,
    max_retries: u32,
    retry_backoff: Duration,
}

impl fmt::Debug for ItaClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItaClient")
            .field("http_client", &self.http_client)
            .field("nonce_url", &self.nonce_url)
            .field("attest_url", &self.attest_url)
            .field("api_key", &"<redacted>")
            .field("max_retries", &self.max_retries)
            .field("retry_backoff", &self.retry_backoff)
            .finish()
    }
}

impl ItaClient {
    pub fn from_config(config: &ItaAttestationConfig) -> Result<Self, ItaClientError> {
        Self::from_config_with_timeout(config, Duration::from_secs(config.timeout_seconds))
    }

    #[cfg(test)]
    pub(crate) fn from_config_for_test(
        config: &ItaAttestationConfig,
        timeout: Duration,
    ) -> Result<Self, ItaClientError> {
        Self::from_config_with_timeout(config, timeout)
    }

    fn from_config_with_timeout(
        config: &ItaAttestationConfig,
        timeout: Duration,
    ) -> Result<Self, ItaClientError> {
        let api_key = config
            .api_key
            .as_deref()
            .ok_or(ItaClientError::MissingCredentials)?;
        let api_key =
            HeaderValue::from_str(api_key).map_err(|_| ItaClientError::InvalidConfig {
                reason: "api key header",
            })?;
        let base_url = Url::parse(config.api_base_url.as_str()).map_err(|_| {
            ItaClientError::InvalidConfig {
                reason: "api base URL",
            }
        })?;
        if base_url.scheme() == "http" && !is_loopback_url(&base_url) {
            return Err(ItaClientError::InvalidConfig {
                reason: "api base URL must use HTTPS for non-loopback hosts",
            });
        }
        let nonce_url =
            base_url
                .join(NONCE_ENDPOINT)
                .map_err(|_| ItaClientError::InvalidConfig {
                    reason: "nonce endpoint URL",
                })?;
        let attest_url =
            base_url
                .join(ATTEST_ENDPOINT)
                .map_err(|_| ItaClientError::InvalidConfig {
                    reason: "attest endpoint URL",
                })?;
        let http_client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|source| ItaClientError::ClientBuild { source })?;

        Ok(Self {
            http_client,
            nonce_url,
            attest_url,
            api_key,
            max_retries: config.max_retries,
            retry_backoff: Duration::from_millis(config.retry_backoff_ms),
        })
    }

    pub async fn get_nonce(&self, request_id: &str) -> Result<ItaNonceResponse, ItaClientError> {
        self.with_retry(|| async { self.get_nonce_once(request_id).await })
            .await
    }

    pub async fn attest(
        &self,
        request_id: &str,
        body: &ItaAttestRequest,
    ) -> Result<ItaAttestResponse, ItaClientError> {
        self.with_retry(|| async { self.attest_once(request_id, body).await })
            .await
    }

    async fn with_retry<T, Fut, Op>(&self, mut operation: Op) -> Result<T, ItaClientError>
    where
        Fut: std::future::Future<Output = Result<T, ItaClientError>>,
        Op: FnMut() -> Fut,
    {
        let mut attempt = 0_u32;
        loop {
            match operation().await {
                Ok(value) => return Ok(value),
                Err(error) if error.is_retryable() && attempt < self.max_retries => {
                    attempt += 1;
                    tokio::time::sleep(self.retry_backoff).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn get_nonce_once(&self, request_id: &str) -> Result<ItaNonceResponse, ItaClientError> {
        let request_id =
            HeaderValue::from_str(request_id).map_err(|_| ItaClientError::InvalidRequestId)?;
        let response = self
            .http_client
            .get(self.nonce_url.clone())
            .header(API_KEY_HEADER, self.api_key.clone())
            .header(ACCEPT, APPLICATION_JSON)
            .header(REQUEST_ID_HEADER, request_id)
            .send()
            .await
            .map_err(transport_error)?;
        let request_id = response_header(response.headers(), REQUEST_ID_HEADER);
        let nonce = parse_json_response::<ItaVerifierNonce>(response, NONCE_BODY_LIMIT).await?;
        nonce
            .validate_wire_encoding()
            .map_err(|source| ItaClientError::InvalidVerifierNonce { source })?;
        Ok(ItaNonceResponse { nonce, request_id })
    }

    async fn attest_once(
        &self,
        request_id: &str,
        body: &ItaAttestRequest,
    ) -> Result<ItaAttestResponse, ItaClientError> {
        let request_id =
            HeaderValue::from_str(request_id).map_err(|_| ItaClientError::InvalidRequestId)?;
        let response = self
            .http_client
            .post(self.attest_url.clone())
            .header(API_KEY_HEADER, self.api_key.clone())
            .header(ACCEPT, APPLICATION_JSON)
            .header(CONTENT_TYPE, APPLICATION_JSON)
            .header(REQUEST_ID_HEADER, request_id)
            .json(body)
            .send()
            .await
            .map_err(transport_error)?;
        let parsed = parse_json_response::<ItaAttestResponse>(response, TOKEN_BODY_LIMIT).await?;
        if parsed.token.is_empty() {
            return Err(ItaClientError::UpstreamResponse {
                reason: "missing token",
            });
        }
        Ok(parsed)
    }
}

#[cfg(test)]
#[path = "client_test_support.rs"]
pub(super) mod client_test_support;

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
