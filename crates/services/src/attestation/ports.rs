use crate::attestation::models::{AttestationError, ChatSignature, GetQuoteResponse};
use async_trait::async_trait;
use inference_providers::AttestationReport;

#[async_trait]
pub trait AttestationServiceTrait: Send + Sync {
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError>;
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
