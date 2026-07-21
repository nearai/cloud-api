use std::time::Duration;

use uuid::Uuid;

use super::{
    AttestationError, AttestationService, ChatSignature, SignatureKind, SignatureLookupResult,
};
use crate::{metrics::consts::*, usage::StopReason};

/// Upper bound on the gateway signature store at end-of-stream. The client is
/// still waiting for the held-back `[DONE]` while this runs, so it must be
/// bounded; on timeout the routing pin is still released and the stream ends
/// without a stored signature (logged by the caller).
pub(in crate::attestation) const STREAM_SIGNATURE_STORE_TIMEOUT: Duration = Duration::from_secs(5);

impl AttestationService {
    pub(in crate::attestation) async fn get_chat_signature_impl(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        let signing_algo = signing_algo
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ecdsa".to_string());

        match self
            .repository
            .get_chat_signature(chat_id, &signing_algo)
            .await
        {
            Ok(signature) => Ok(SignatureLookupResult::Found(signature)),
            Err(AttestationError::SignatureNotFound(_)) => {
                if let Some(provider) = self
                    .inference_provider_pool
                    .get_provider_by_chat_id(chat_id)
                    .await
                {
                    if !provider.supports_chat_signatures() {
                        return Ok(SignatureLookupResult::Unavailable {
                            error_code: "SIGNATURE_UNSUPPORTED".to_string(),
                            message: "This model's responses are integrity-protected by an \
                                      end-to-end encrypted channel, not a per-response signature; \
                                      there is no signature to retrieve."
                                .to_string(),
                        });
                    }
                }
                self.check_fallback_conditions(chat_id, &signing_algo).await
            }
            Err(e) => Err(e),
        }
    }

    pub(in crate::attestation) async fn store_chat_signature_from_provider_impl(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError> {
        let start_time = std::time::Instant::now();
        let provider = self
            .inference_provider_pool
            .get_provider_by_chat_id(chat_id)
            .await
            .ok_or_else(|| {
                AttestationError::ProviderError(format!("No provider found for chat_id: {chat_id}"))
            })?;

        if !provider.supports_chat_signatures() {
            return Ok(());
        }

        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");
        let result: Result<(), AttestationError> = async {
            for algo in ["ecdsa", "ed25519"] {
                let provider_signature = provider
                    .get_signature(chat_id, Some(algo.to_string()))
                    .await
                    .map_err(|e| {
                        // The error string embeds the backend URL on connection
                        // failures — without it (and chat_id) these events are
                        // impossible to attribute to a model/backend.
                        tracing::error!(
                            %chat_id,
                            error = %e,
                            "Failed to get chat signature from provider for algorithm: {}",
                            algo
                        );
                        let duration = start_time.elapsed();
                        self.metrics_service.record_count(
                            METRIC_VERIFICATION_FAILURE,
                            1,
                            &[&format!("{TAG_REASON}:{REASON_INFERENCE_ERROR}"), &env_tag],
                        );
                        self.metrics_service.record_latency(
                            METRIC_VERIFICATION_DURATION,
                            duration,
                            &[&env_tag],
                        );
                        AttestationError::ProviderError(e.to_string())
                    })?;
                let signature = ChatSignature {
                    text: provider_signature.text,
                    signature: provider_signature.signature,
                    signing_address: provider_signature.signing_address,
                    signing_algo: provider_signature.signing_algo,
                    signature_kind: Some(SignatureKind::ProviderTee),
                };

                self.repository
                    .add_chat_signature(chat_id, signature)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            %chat_id,
                            error = %e,
                            "Failed to store chat signature in repository for algorithm: {}",
                            algo
                        );
                        let duration = start_time.elapsed();
                        self.metrics_service.record_count(
                            METRIC_VERIFICATION_FAILURE,
                            1,
                            &[&format!("{TAG_REASON}:{REASON_REPOSITORY_ERROR}"), &env_tag],
                        );
                        self.metrics_service.record_latency(
                            METRIC_VERIFICATION_DURATION,
                            duration,
                            &[&env_tag],
                        );
                        AttestationError::RepositoryError(e.to_string())
                    })?;
            }
            Ok(())
        }
        .await;

        provider.unpin_chat_connection(chat_id);
        result?;

        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[&env_tag]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[&env_tag]);
        Ok(())
    }

    pub(in crate::attestation) async fn store_chat_signature_impl(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(chat_id, "chat", request_hash, response_hash)
            .await
    }

    /// Store the gateway signature for a stream and then release the
    /// provider-pool signature-fetch routing pin, mirroring the lifecycle
    /// ownership of `store_chat_signature_from_provider_impl` (which unpins
    /// after the provider fetch). The store is bounded by
    /// [`STREAM_SIGNATURE_STORE_TIMEOUT`] *inside* this method so the unpin
    /// runs even when the store hangs — an outer timeout would drop the
    /// future and leak the pin.
    pub(in crate::attestation) async fn store_chat_signature_and_unpin_impl(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        let result = match tokio::time::timeout(
            STREAM_SIGNATURE_STORE_TIMEOUT,
            self.store_chat_signature_impl(chat_id, request_hash, response_hash),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(AttestationError::InternalError(format!(
                "Timed out storing gateway chat signature for {chat_id}"
            ))),
        };
        self.release_chat_signature_pin_impl(chat_id).await;
        result
    }

    /// Release the provider-pool signature-fetch routing pin for `chat_id`.
    /// Called on gateway-signed streams (where the provider signature fetch —
    /// and its post-fetch unpin — is skipped) and on errored streams that
    /// store nothing.
    pub(in crate::attestation) async fn release_chat_signature_pin_impl(&self, chat_id: &str) {
        if let Some(provider) = self
            .inference_provider_pool
            .get_provider_by_chat_id(chat_id)
            .await
        {
            provider.unpin_chat_connection(chat_id);
        }
    }

    pub(in crate::attestation) async fn store_response_signature_impl(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(response_id, "response", request_hash, response_hash)
            .await
    }

    async fn check_fallback_conditions(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<SignatureLookupResult, AttestationError> {
        let stop_reason = if chat_id.starts_with("resp_") {
            let uuid_str = chat_id.strip_prefix("resp_").unwrap_or(chat_id);
            let response_uuid = match Uuid::parse_str(uuid_str) {
                Ok(uuid) => uuid,
                Err(_) => {
                    return Err(AttestationError::SignatureNotFound(format!(
                        "{}:{}",
                        chat_id, signing_algo
                    )));
                }
            };
            self.usage_repository
                .get_stop_reason_by_response_id(response_uuid)
                .await
                .map_err(|e| AttestationError::RepositoryError(e.to_string()))?
        } else {
            self.usage_repository
                .get_stop_reason_by_provider_request_id(chat_id)
                .await
                .map_err(|e| AttestationError::RepositoryError(e.to_string()))?
        };

        match stop_reason {
            Some(StopReason::ClientDisconnect) => Ok(SignatureLookupResult::Unavailable {
                error_code: "STREAM_DISCONNECTED".to_string(),
                message: "Verification not available due to disconnection.".to_string(),
            }),
            _ => Err(AttestationError::SignatureNotFound(format!(
                "{}:{}",
                chat_id, signing_algo
            ))),
        }
    }
}
