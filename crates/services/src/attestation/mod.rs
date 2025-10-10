pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature};
use std::sync::Arc;

use async_trait::async_trait;
use inference_providers::{AttestationReport, InferenceProvider};

use crate::{
    attestation::{models::GetQuoteResponse, ports::AttestationRepository},
    inference_provider_pool::InferenceProviderPool,
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
impl ports::AttestationServiceTrait for AttestationService {
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError> {
        // Only get from database
        self.repository.get_chat_signature(chat_id).await
    }

    async fn store_chat_signature_from_provider(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError> {
        // Get the provider for this chat
        let provider = self
            .inference_provider_pool
            .get_provider_by_chat_id(chat_id)
            .await
            .ok_or_else(|| {
                AttestationError::ProviderError(format!(
                    "No provider found for chat_id: {}",
                    chat_id
                ))
            })?;

        // Fetch signature from provider
        let provider_signature = provider.get_signature(chat_id).await.map_err(|e| {
            tracing::error!("Failed to get chat signature from provider: {:?}", e);
            AttestationError::ProviderError(e.to_string())
        })?;

        let signature = ChatSignature {
            text: provider_signature.text,
            signature: provider_signature.signature,
            signing_address: provider_signature.signing_address,
            signing_algo: provider_signature.signing_algo,
        };

        // Store in repository
        self.repository
            .add_chat_signature(chat_id, signature)
            .await
            .map_err(|e| {
                tracing::error!("Failed to store chat signature in repository: {:?}", e);
                AttestationError::RepositoryError(e.to_string())
            })?;

        Ok(())
    }

    async fn get_attestation_report(
        &self,
        model: String,
        signing_algo: Option<String>,
    ) -> Result<AttestationReport, AttestationError> {
        self.inference_provider_pool
            .get_attestation_report(model, signing_algo)
            .await
            .map_err(|e| AttestationError::ProviderError(e.to_string()))
    }

    async fn get_quote(&self) -> Result<GetQuoteResponse, AttestationError> {
        let client = dstack_client::DstackClient::new(None);
        let quote = client
            .get_quote(vec![])
            .await
            .map_err(|e| AttestationError::ClientError(e.to_string()))?;
        Ok(GetQuoteResponse::from(quote))
    }
}
