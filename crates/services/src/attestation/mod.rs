pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature};
use std::sync::Arc;

use async_trait::async_trait;
use hex;
use rand::RngCore;

use crate::{
    attestation::{
        models::{AttestationReport, DstackCpuQuote, VpcInfo},
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

/// Load VPC information from environment variables
pub fn load_vpc_info() -> Option<VpcInfo> {
    // Read VPC server app ID from environment
    let vpc_server_app_id = std::env::var("VPC_SERVER_APP_ID").ok();

    // Read VPC hostname from file
    let vpc_hostname = if let Ok(path) = std::env::var("VPC_HOSTNAME_FILE") {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    // Only return Some if at least one field is present
    if vpc_server_app_id.is_some() || vpc_hostname.is_some() {
        Some(VpcInfo {
            vpc_server_app_id,
            vpc_hostname,
        })
    } else {
        None
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
                AttestationError::ProviderError(format!("No provider found for chat_id: {chat_id}"))
            })?;

        // Fetch signature from provider
        let provider_signature = provider.get_signature(chat_id).await.map_err(|e| {
            tracing::error!("Failed to get chat signature from provider");
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
                tracing::error!("Failed to store chat signature in repository");
                AttestationError::RepositoryError(e.to_string())
            })?;

        Ok(())
    }

    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
    ) -> Result<AttestationReport, AttestationError> {
        // Resolve model name (could be an alias) and get model details
        let mut all_attestations = vec![];
        // Create a nonce if none was provided
        let nonce = match nonce {
            Some(n) => n,
            None => {
                let mut nonce_bytes = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
                let generated_nonce = nonce_bytes
                    .into_iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                tracing::debug!(
                    "No nonce provided for attestation report, generated nonce: {}",
                    generated_nonce
                );
                generated_nonce
            }
        };
        if let Some(model) = model {
            let resolved_model = self
                .models_repository
                .resolve_and_get_model(&model)
                .await
                .map_err(|e| {
                    AttestationError::ProviderError(format!("Failed to resolve model: {e}"))
                })?
                .ok_or_else(|| {
                    AttestationError::ProviderError(format!(
                        "Model '{model}' not found. It's not a valid model name or alias."
                    ))
                })?;

            let canonical_name = &resolved_model.model_name;

            // Log if we resolved an alias
            if canonical_name != &model {
                tracing::debug!(
                    requested_model = %model,
                    canonical_model = %canonical_name,
                    "Resolved alias to canonical model name for attestation report"
                );
            }

            all_attestations = self
                .inference_provider_pool
                .get_attestation_report(
                    canonical_name.clone(),
                    signing_algo,
                    Some(nonce.clone()),
                    signing_address,
                )
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }

        // Load VPC info
        let vpc = load_vpc_info();

        let gateway_attestation;
        if let Ok(_dev) = std::env::var("DEV") {
            gateway_attestation = DstackCpuQuote {
                intel_quote: "0x1234567890abcdef".to_string(),
                event_log: "0x1234567890abcdef".to_string(),
                report_data: "0x1234567890abcdef".to_string(),
                request_nonce: nonce.clone(),
                info: serde_json::json!({
                    "app_id": "dev-app-id",
                    "instance_id": "dev-instance-id",
                    "app_cert": "dev-app-cert",
                    "tcb_info": {},
                    "app_name": "dev-app-name",
                    "device_id": "dev-device-id",
                    "mr_aggregated": "dev-mr-aggregated",
                    "os_image_hash": "dev-os-image-hash",
                    "key_provider_info": "dev-key-provider-info",
                    "compose_hash": "dev-compose-hash",
                    "vm_config": {},
                }),
                vpc,
            };
        } else {
            let client = dstack_client::DstackClient::new(None);
            // Decode hex string nonce to bytes (nonce should be 64 hex chars = 32 bytes)
            let nonce_bytes = hex::decode(&nonce).map_err(|e| {
                tracing::error!("Failed to decode nonce hex string: {}", e);
                AttestationError::InvalidParameter(format!("Invalid nonce format: {e}"))
            })?;

            if nonce_bytes.len() != 32 {
                return Err(AttestationError::InvalidParameter(format!(
                    "Nonce must be exactly 32 bytes, got {} bytes",
                    nonce_bytes.len()
                )));
            }

            // Construct 64-byte report_data: first 32 bytes are zeros for gateway attestation,
            // last 32 bytes are the nonce
            let mut report_data = vec![0u8; 64];
            // Place nonce in the last 32 bytes
            report_data[32..64].copy_from_slice(&nonce_bytes);

            let info = client.info().await.map_err(|_| {
                tracing::error!(
                    "Failed to get cloud API attestation info, are you running in a CVM?"
                );
                AttestationError::InternalError(
                    "failed to get cloud API attestation info".to_string(),
                )
            })?;

            let cpu_quote = client.get_quote(report_data).await.map_err(|_| {
                tracing::error!("Failed to get cloud API attestation, are you running in a CVM?");
                AttestationError::InternalError("failed to get cloud API attestation".to_string())
            })?;
            gateway_attestation = DstackCpuQuote::from_quote_and_nonce(vpc, info, cpu_quote, nonce);
        }

        Ok(AttestationReport {
            gateway_attestation,
            all_attestations,
        })
    }
}
