use crate::attestation::models::{AttestationError, AttestationReport, ChatSignature};
use async_trait::async_trait;

#[async_trait]
pub trait AttestationServiceTrait: Send + Sync {
    /// Get a chat signature from the database only
    /// signing_algo: Optional signing algorithm ("ed25519" or "ecdsa"), defaults to "ed25519" if None
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, AttestationError>;

    /// Fetch signature from provider and store it in the database
    /// This should be called when a completion finishes
    async fn store_chat_signature_from_provider(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError>;

    /// Store a response signature directly (for response streams)
    /// Creates a signature with text format "request_hash:response_hash"
    /// signing_algo: Optional signing algorithm ("ed25519" or "ecdsa"), defaults to "ed25519"
    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
        signing_algo: Option<String>,
    ) -> Result<(), AttestationError>;

    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
    ) -> Result<AttestationReport, AttestationError>;
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
        signing_algo: Option<String>,
    ) -> Result<ChatSignature, AttestationError>;
}
