pub mod models;
pub use models::ChatSignature;
use std::sync::Arc;

use async_trait::async_trait;
use inference_providers::{AttestationReport, InferenceProvider};

use crate::{
    attestation::ports::AttestationRepository, inference_provider_pool::InferenceProviderPool,
    CompletionError,
};

pub mod ports;

pub struct AttestationService {
    pub repository: Arc<dyn AttestationRepository + Send + Sync>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
}

impl AttestationService {
    pub fn new(
        repository: Arc<dyn AttestationRepository + Send + Sync>,
        inference_provider_pool: Arc<InferenceProviderPool>,
    ) -> Self {
        Self {
            repository,
            inference_provider_pool,
        }
    }
}

#[async_trait]
impl ports::AttestationService for AttestationService {
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, CompletionError> {
        if let Some(provider) = self
            .inference_provider_pool
            .get_provider_by_chat_id(chat_id)
            .await
        {
            let provider_signature = provider.get_signature(chat_id).await.map_err(|e| {
                tracing::error!("Failed to get chat signature: {:?}", e);
                CompletionError::ProviderError(e.to_string())
            })?;
            let signature = ChatSignature {
                text: provider_signature.text,
                signature: provider_signature.signature,
                signing_address: provider_signature.signing_address,
                signing_algo: provider_signature.signing_algo,
            };
            self.repository
                .add_chat_signature(chat_id, signature.clone())
                .await
                .map_err(|e| {
                    tracing::error!("Failed to add chat signature: {:?}", e);
                    CompletionError::InternalError(e.to_string())
                })?;
            return Ok(signature);
        }
        self.repository.get_chat_signature(chat_id).await
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
    ) -> Result<AttestationReport, CompletionError> {
        self.inference_provider_pool
            .get_attestation_report(model, signing_algo)
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))
    }
}
