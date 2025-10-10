use crate::attestation::models::{AttestationError, ChatSignature, GetQuoteResponse};
use async_trait::async_trait;
use inference_providers::AttestationReport;

#[async_trait]
pub trait AttestationServiceTrait: Send + Sync {
    /// Get a chat signature from the database only
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError>;

    /// Fetch signature from provider and store it in the database
    /// This should be called when a completion finishes
    async fn store_chat_signature_from_provider(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError>;

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
    ) -> Result<AttestationReport, AttestationError>;
    async fn get_quote(&self) -> Result<GetQuoteResponse, AttestationError>;
}

#[async_trait]
pub trait AttestationRepository: Send + Sync {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError>;
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError>;
}
