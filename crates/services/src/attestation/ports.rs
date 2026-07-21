use crate::attestation::ita::{ItaTokenQuery, ItaTokenResponse};
use crate::attestation::models::{
    AttestationError, AttestationReport, ChatSignature, SignatureLookupResult,
};
use async_trait::async_trait;
use inference_providers::ProviderTier;

#[async_trait]
pub trait AttestationServiceTrait: Send + Sync {
    /// Get a chat signature from the database, with fallback for client disconnect
    /// signing_algo: Optional signing algorithm ("ed25519" or "ecdsa"), defaults to "ecdsa" if None
    /// Returns SignatureLookupResult which can be Found (signature) or Unavailable (client disconnect)
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError>;

    /// Fetch signature from provider and store it in the database
    /// This should be called when a completion finishes
    async fn store_chat_signature_from_provider(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError>;

    /// Store a chat signature directly over gateway-emitted bytes.
    /// Creates a signature with text format "request_hash:response_hash"
    /// and stores both ECDSA and ED25519 signatures.
    async fn store_chat_signature(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError>;

    /// Store a gateway chat signature and then release the provider-pool
    /// signature-fetch routing pin for `chat_id`, mirroring the lifecycle
    /// ownership of [`Self::store_chat_signature_from_provider`]: the pin is
    /// released whether the store succeeds, fails, or times out, so the
    /// provider's chat_id → backend map cannot grow unboundedly on
    /// gateway-signed streams (where the provider signature fetch — and its
    /// post-fetch unpin — is skipped).
    async fn store_chat_signature_and_unpin(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        let result = self
            .store_chat_signature(chat_id, request_hash, response_hash)
            .await;
        self.release_chat_signature_pin(chat_id).await;
        result
    }

    /// Release the provider-pool signature-fetch routing pin for `chat_id`
    /// without storing a signature — the errored-stream path, where there is
    /// nothing to sign but the pin still has to be dropped. Default no-op for
    /// implementations without a provider pool (mocks).
    async fn release_chat_signature_pin(&self, _chat_id: &str) {}

    /// Store a response signature directly (for response streams)
    /// Creates a signature with text format "request_hash:response_hash"
    /// Stores both ECDSA and ED25519 signatures
    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError>;

    /// Fetch a hardware attestation report.
    ///
    /// `provider_filter`: when `Some`, only the matching trust tier is queried.
    /// `None` keeps the existing behaviour (first successful provider wins).
    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
        provider_filter: Option<ProviderTier>,
    ) -> Result<AttestationReport, AttestationError>;

    async fn get_ita_attestation_token(
        &self,
        query: ItaTokenQuery,
    ) -> Result<ItaTokenResponse, AttestationError>;

    /// Verify a VPC shared secret signature
    async fn verify_vpc_signature(
        &self,
        timestamp: i64,
        signature: String,
    ) -> Result<bool, AttestationError>;
}

#[async_trait]
pub trait AttestationRepository: Send + Sync {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError>;
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError>;
}
