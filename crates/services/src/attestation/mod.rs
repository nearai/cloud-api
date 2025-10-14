pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature};
use std::sync::Arc;

use async_trait::async_trait;
use inference_providers::InferenceProvider;

use crate::{
    attestation::{
        models::{AttestationReport, DstackCpuQuote},
        ports::AttestationRepository,
    },
    inference_provider_pool::InferenceProviderPool,
    models::ModelsRepository,
};

pub mod ports;

pub struct AttestationService {
    pub repository: Arc<dyn AttestationRepository + Send + Sync>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
}

impl AttestationService {
    pub fn new(
        repository: Arc<dyn AttestationRepository + Send + Sync>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        Self {
            repository,
            inference_provider_pool,
            models_repository,
        }
    }
}

#[async_trait]
impl ports::AttestationServiceTrait for AttestationService {
    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError> {
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
        model: Option<String>,
        signing_algo: Option<String>,
    ) -> Result<AttestationReport, AttestationError> {
        // Resolve model name (could be an alias) to canonical name
        let mut model_attestations = vec![];
        if let Some(model) = model {
            let canonical_name = self
                .models_repository
                .resolve_to_canonical_name(&model)
                .await
                .map_err(|e| {
                    AttestationError::ProviderError(format!("Failed to resolve model name: {}", e))
                })?;

            // Log if we resolved an alias
            if canonical_name != model {
                tracing::debug!(
                    requested_model = %model,
                    canonical_model = %canonical_name,
                    "Resolved alias to canonical model name for attestation report"
                );
            }

            model_attestations = self
                .inference_provider_pool
                .get_attestation_report(canonical_name, signing_algo)
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }
        let gateway_attestation;
        if let Ok(_dev) = std::env::var("DEV") {
            gateway_attestation = DstackCpuQuote {
                quote: "0x1234567890abcdef".to_string(),
                event_log: "0x1234567890abcdef".to_string(),
            };
        } else {
            let client = dstack_client::DstackClient::new(None);
            gateway_attestation = client
                .get_quote(vec![8])
                .await
                .map_err(|e| {
                    tracing::error!(
                        "Failed to get cloud API attestation, are you running in a CVM? {:?}",
                        e
                    );
                    AttestationError::InternalError(
                        "failed to get cloud API attestation".to_string(),
                    )
                })?
                .into();
        }

        Ok(AttestationReport {
            gateway_attestation,
            model_attestations,
        })
    }
}
