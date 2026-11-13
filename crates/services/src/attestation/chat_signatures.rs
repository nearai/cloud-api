use std::time::Duration;

use super::{
    AttestationError, AttestationService, ChatSignature, SignatureKind, SignatureLookupResult,
    SignatureOwner,
};
use crate::metrics::consts::*;

/// Upper bound on signature finalization at end-of-stream. The client is
/// still waiting for the held-back `[DONE]` while this runs, so it must be
/// bounded; on timeout the routing pin is still released and the stream ends
/// without a stored signature (logged by the caller).
pub(crate) const STREAM_SIGNATURE_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reserve the final second of the streaming deadline for persisting fetched
/// signatures. An outer timeout would otherwise drop an already-fetched ECDSA
/// signature if the ED25519 fetch stalls before the batch can be written.
pub(in crate::attestation) const PROVIDER_SIGNATURE_FETCH_TIMEOUT: Duration =
    STREAM_SIGNATURE_STORE_TIMEOUT.saturating_sub(Duration::from_secs(1));

/// `error_code` returned when the caller's own stream ended with a client
/// disconnect and no signature was stored.
pub(crate) const STREAM_DISCONNECTED: &str = "STREAM_DISCONNECTED";
const STREAM_DISCONNECTED_MESSAGE: &str = "Verification not available due to disconnection.";

impl AttestationService {
    /// Only a row stored for `owner` is returned. When there is none, a usage
    /// row of `owner` for `chat_id` that ended with a client disconnect gives
    /// [`SignatureLookupResult::Unavailable`] with [`STREAM_DISCONNECTED`].
    /// Otherwise a missing row, a row of another owner and a row with no owner
    /// all give the same `SignatureNotFound`, with no reason attached.
    pub(in crate::attestation) async fn get_chat_signature_impl(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        let signing_algo = signing_algo
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ecdsa".to_string());

        match self
            .repository
            .get_chat_signature(owner, chat_id, &signing_algo)
            .await
        {
            Ok(signature) => Ok(SignatureLookupResult::Found(signature)),
            Err(not_found @ AttestationError::SignatureNotFound(_)) => {
                if self.owner_stream_disconnected(owner, chat_id).await {
                    Ok(SignatureLookupResult::Unavailable {
                        error_code: STREAM_DISCONNECTED.to_string(),
                        message: STREAM_DISCONNECTED_MESSAGE.to_string(),
                    })
                } else {
                    Err(not_found)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Whether `owner` has a usage row for `chat_id` that ended with a client
    /// disconnect. A lookup failure answers `false`, so the caller reports the
    /// signature as not found.
    async fn owner_stream_disconnected(&self, owner: SignatureOwner, chat_id: &str) -> bool {
        match self
            .usage_repository
            .has_client_disconnect_usage(owner.organization_id, owner.workspace_id, chat_id)
            .await
        {
            Ok(disconnected) => disconnected,
            Err(_) => {
                tracing::warn!(
                    organization_id = %owner.organization_id,
                    workspace_id = %owner.workspace_id,
                    error_kind = "usage_lookup_failed",
                    "Failed to check stop reason for a missing signature"
                );
                false
            }
        }
    }

    pub(in crate::attestation) async fn store_chat_signature_from_provider_impl(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        fetch_deadline: Option<tokio::time::Instant>,
    ) -> Result<(), AttestationError> {
        let start_time = std::time::Instant::now();
        let provider = self
            .inference_provider_pool
            .get_provider_by_chat_id(chat_id)
            .await
            .ok_or_else(|| {
                AttestationError::ProviderError("No provider found for chat signature".to_string())
            })?;

        if !provider.supports_chat_signatures() {
            // The pool pins the serving connection before it knows whether the
            // selected provider exposes per-response signatures. Providers
            // such as Chutes deliberately do not, but their normal completion
            // path must still drain that pin.
            provider.unpin_chat_connection(chat_id);
            return Ok(());
        }

        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");
        // Fetch both algorithms independently. A slow or unavailable legacy
        // ECDSA signature must not consume the shared streaming deadline before
        // the SDK-default Ed25519 lookup gets a chance to run. Keep the result
        // order deterministic so the repository batch remains stable.
        let fetch_signature = |algo: &'static str| {
            let provider = provider.clone();
            async move {
                let fetch = async {
                    provider
                        .get_signature(chat_id, Some(algo.to_string()))
                        .await
                        .map_err(|e| AttestationError::ProviderError(e.to_string()))
                };
                let result = match fetch_deadline {
                    Some(deadline) => match tokio::time::timeout_at(deadline, fetch).await {
                        Ok(result) => result,
                        Err(_) => Err(AttestationError::ProviderError(format!(
                            "Timed out fetching chat signature for algorithm: {algo}"
                        ))),
                    },
                    None => fetch.await,
                };
                (algo, result)
            }
        };
        let (ecdsa, ed25519) = tokio::join!(fetch_signature("ecdsa"), fetch_signature("ed25519"));

        let mut signatures = Vec::with_capacity(2);
        let mut fetch_error = None;
        for (algo, result) in [ecdsa, ed25519] {
            match result {
                Ok(provider_signature) => signatures.push(ChatSignature {
                    text: provider_signature.text,
                    signature: provider_signature.signature,
                    signing_address: provider_signature.signing_address,
                    signing_algo: provider_signature.signing_algo,
                    signature_kind: Some(SignatureKind::ProviderTee),
                }),
                Err(error) => {
                    tracing::error!(
                        signing_algo = algo,
                        error_kind = error.kind(),
                        "Failed to get chat signature from provider"
                    );
                    if fetch_error.is_none() {
                        fetch_error = Some(error);
                    }
                }
            }
        }
        let fetched = if let Some(error) = fetch_error {
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
            Err(error)
        } else {
            Ok(())
        };

        // Store whatever was fetched even if either algorithm failed or hit the
        // fetch deadline, leaving time before the outer stream timeout. Both
        // algorithms land in one statement:
        // this runs before the client sees `[DONE]`, so every saved round
        // trip is user-visible.
        let stored: Result<(), AttestationError> = if signatures.is_empty() {
            Ok(())
        } else {
            self.repository
                .add_chat_signatures(owner, chat_id, signatures)
                .await
                .inspect_err(|e| {
                    tracing::error!(
                        organization_id = %owner.organization_id,
                        workspace_id = %owner.workspace_id,
                        error_kind = e.kind(),
                        "Failed to store chat signatures in repository"
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
                })
        };
        let result = fetched.and(stored);

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
        owner: SignatureOwner,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(owner, chat_id, "chat", request_hash, response_hash)
            .await
    }

    /// Store a gateway signature and then release the provider-pool
    /// signature-fetch routing pin, mirroring the lifecycle ownership of
    /// `store_chat_signature_from_provider_impl` (which unpins after the
    /// provider fetch). This is used by non-streaming Gateway-signing paths;
    /// streaming cleanup is owned by `InterceptStream`. The store is bounded by
    /// [`STREAM_SIGNATURE_STORE_TIMEOUT`] *inside* this method so the unpin
    /// runs even when the store hangs — an outer timeout would drop the
    /// future and leak the pin.
    pub(in crate::attestation) async fn store_chat_signature_and_unpin_impl(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        let result = match tokio::time::timeout(
            STREAM_SIGNATURE_STORE_TIMEOUT,
            self.store_chat_signature_impl(owner, chat_id, request_hash, response_hash),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(AttestationError::InternalError(
                "Timed out storing gateway chat signature".to_string(),
            )),
        };
        self.release_chat_signature_pin_impl(chat_id).await;
        result
    }

    /// Release the provider-pool signature-fetch routing pin for `chat_id`.
    /// Called by non-streaming response-rewrite paths and by completion-stream
    /// finalization when no provider signature is stored.
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
        owner: SignatureOwner,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(owner, response_id, "response", request_hash, response_hash)
            .await
    }
}
