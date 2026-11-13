//! Request-time collection of one report per backend index.
//!
//! Discovery maintains routing keys and TLS pins, but its reports use internal
//! nonces. A public request must collect evidence for its own challenge instead.

use super::fleet::Fleet;
use crate::rotation::{self, CountFetch, UrlParts};
use crate::spki_verifier::FingerprintState;
use crate::AttestationError;
use futures_util::{stream, StreamExt, TryStreamExt};
use reqwest::Client;
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const COLLECTION_CONCURRENCY: usize = 8;
const COUNT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Serialize)]
pub(super) struct ReportQuery {
    model: String,
    signing_algo: Option<String>,
    nonce: Option<String>,
    signing_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_tls_fingerprint: Option<bool>,
}

impl ReportQuery {
    pub(super) fn new(
        model: String,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Self {
        Self {
            model,
            signing_algo,
            nonce,
            signing_address,
            include_tls_fingerprint: include_tls_fingerprint.then_some(true),
        }
    }
}

impl Fleet {
    pub(super) async fn fetch_attestation_at(
        &self,
        client: &Client,
        base_url: &str,
        query: &ReportQuery,
    ) -> Result<Map<String, Value>, AttestationError> {
        let url = format!(
            "{}/v1/attestation/report?{}",
            base_url.trim_end_matches('/'),
            serde_urlencoded::to_string(query)
                .map_err(|e| AttestationError::Unknown(e.to_string()))?
        );
        let headers = self.build_headers().map_err(AttestationError::FetchError)?;
        let response = client
            .get(url)
            .headers(headers)
            .timeout(self.config.control_timeout())
            .send()
            .await
            .map_err(|e| AttestationError::FetchError(e.without_url().to_string()))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            if query.signing_address.is_none() {
                return Err(AttestationError::FetchError(format!(
                    "Backend returned no attestation report for model {} (HTTP 404)",
                    query.model
                )));
            }
            return Err(AttestationError::SigningAddressNotFound(
                query.signing_address.clone().unwrap_or_default(),
            ));
        }
        if !response.status().is_success() {
            return Err(AttestationError::FetchError(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_else(|error| format!(
                    "Failed to read error response body: {}",
                    error.without_url()
                ))
            )));
        }
        response
            .json()
            .await
            .map_err(|e| AttestationError::InvalidResponse(e.without_url().to_string()))
    }

    pub(super) async fn collect_attestation_reports(
        &self,
        query: ReportQuery,
    ) -> Result<Vec<Map<String, Value>>, AttestationError> {
        let Some(parts) = self.attestation_rotation_parts() else {
            return self
                .fetch_attestation_at(&self.client, &self.config.base_url, &query)
                .await
                .map(|report| vec![report]);
        };

        // The registry's base-domain certificate is not a model CVM's SPKI.
        // Its client uses normal certificate verification and sends no token.
        let count_client =
            self.report_client(Arc::new(RwLock::new(FingerprintState::Bootstrap)))?;
        // A fresh client ensures that each indexed hostname opens a connection
        // for this collection instead of reusing an older index-to-backend map.
        // Model requests retain the same fingerprint policy as the Fleet.
        let model_client = self.report_client(self.fingerprint_state.clone())?;

        self.collect_indexed_reports(parts, &count_client, &model_client, &query)
            .await
    }

    fn report_client(
        &self,
        fingerprint_state: Arc<RwLock<FingerprintState>>,
    ) -> Result<Client, AttestationError> {
        Client::builder()
            .use_preconfigured_tls(self.tls_roots.build_config(fingerprint_state))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(self.config.control_timeout())
            .build()
            .map_err(|e| AttestationError::FetchError(e.to_string()))
    }

    async fn collect_indexed_reports(
        &self,
        parts: &UrlParts,
        count_client: &Client,
        model_client: &Client,
        query: &ReportQuery,
    ) -> Result<Vec<Map<String, Value>>, AttestationError> {
        tokio::time::timeout(
            self.config.control_timeout(),
            self.collect_indexed_reports_inner(parts, count_client, model_client, query),
        )
        .await
        .map_err(|_| AttestationError::FetchError("Attestation collection timed out".into()))?
    }

    async fn collect_indexed_reports_inner(
        &self,
        parts: &UrlParts,
        count_client: &Client,
        model_client: &Client,
        query: &ReportQuery,
    ) -> Result<Vec<Map<String, Value>>, AttestationError> {
        let count = read_backend_count(count_client, parts).await?;
        if count == 0 {
            return Err(AttestationError::ProviderNotFound(query.model.clone()));
        }
        if count > rotation::MAX_FANOUT {
            return Err(AttestationError::FetchError(format!(
                "Backend count {count} exceeds attestation collection limit {}",
                rotation::MAX_FANOUT
            )));
        }

        let reports: Vec<Option<Map<String, Value>>> = stream::iter(0..count)
            .map(|index| async move {
                let url = rotation::rotation_base_url(parts, index as u64).ok_or_else(|| {
                    AttestationError::FetchError("Cannot build backend attestation URL".into())
                })?;
                match self
                    .fetch_attestation_at(model_client, url.as_str(), query)
                    .await
                {
                    Ok(report) => Ok(Some(report)),
                    Err(AttestationError::SigningAddressNotFound(_))
                        if query.signing_address.is_some() =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(AttestationError::FetchError(format!(
                        "Backend index {index}: {error}"
                    ))),
                }
            })
            .buffered(COLLECTION_CONCURRENCY)
            .try_collect()
            .await?;

        // Detect an observable membership change instead of publishing a
        // known-partial set. Equal counts cannot rule out same-size membership
        // replacement; index routing is a discovery observation, not an atomic
        // fleet snapshot.
        let rechecked_count = read_backend_count(count_client, parts)
            .await
            .map_err(|error| {
                AttestationError::FetchError(format!(
                    "Backend count recheck failed after querying {count} backends: {error}"
                ))
            })?;
        if rechecked_count != count {
            return Err(AttestationError::FetchError(
                "Backend count changed during attestation collection".into(),
            ));
        }
        let reports: Vec<_> = reports.into_iter().flatten().collect();
        if reports.is_empty() {
            return Err(AttestationError::SigningAddressNotFound(
                query.signing_address.clone().unwrap_or_default(),
            ));
        }
        // Replicas may share a signing key. Preserve every index's evidence,
        // including its own compose-manager envelope, rather than deduplicating
        // by signer or copying control-plane evidence between replicas.
        Ok(reports)
    }
}

async fn read_backend_count(client: &Client, parts: &UrlParts) -> Result<usize, AttestationError> {
    match rotation::fetch_backend_count(client, parts, COUNT_TIMEOUT).await {
        CountFetch::Ok(count) => Ok(count),
        CountFetch::Err(reason) => Err(AttestationError::FetchError(reason)),
    }
}

#[cfg(test)]
mod tests;
