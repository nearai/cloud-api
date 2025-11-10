pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature};
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hex;
use k256::ecdsa::{
    Signature as EcdsaSignature, SigningKey as EcdsaSigningKey, VerifyingKey as EcdsaVerifyingKey,
};
use rand::rngs::OsRng;
use rand::RngCore;

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
    ed25519_signing_key: Arc<SigningKey>,
    ed25519_verifying_key: Arc<VerifyingKey>,
    ecdsa_signing_key: Arc<EcdsaSigningKey>,
    ecdsa_verifying_key: Arc<EcdsaVerifyingKey>,
}

impl AttestationService {
    pub fn new(
        repository: Arc<dyn AttestationRepository + Send + Sync>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
    ) -> Self {
        let mut csprng = OsRng;

        // Generate ed25519 key pair on startup
        let ed25519_signing_key = SigningKey::generate(&mut csprng);
        let ed25519_verifying_key = ed25519_signing_key.verifying_key();
        let ed25519_signing_key = Arc::new(ed25519_signing_key);
        let ed25519_verifying_key = Arc::new(ed25519_verifying_key);

        let ed25519_address = hex::encode(ed25519_verifying_key.as_bytes());
        tracing::info!(
            "Generated ed25519 key pair for response signing. Public key (signing address): 0x{}",
            ed25519_address
        );

        // Generate ECDSA (secp256k1) key pair on startup
        let ecdsa_signing_key = EcdsaSigningKey::random(&mut csprng);
        let ecdsa_verifying_key = *ecdsa_signing_key.verifying_key();
        let ecdsa_signing_key = Arc::new(ecdsa_signing_key);
        let ecdsa_verifying_key = Arc::new(ecdsa_verifying_key);

        // ECDSA public key is 33 bytes (compressed) or 65 bytes (uncompressed)
        // We'll use the compressed format (33 bytes) and encode it
        let ecdsa_address = hex::encode(ecdsa_verifying_key.to_sec1_bytes());
        tracing::info!(
            "Generated ECDSA (secp256k1) key pair for response signing. Public key (signing address): 0x{}",
            ecdsa_address
        );

        Self {
            repository,
            inference_provider_pool,
            models_repository,
            ed25519_signing_key,
            ed25519_verifying_key,
            ecdsa_signing_key,
            ecdsa_verifying_key,
        }
    }

    /// Get the signing address (public key) as a hex string for the specified algorithm
    pub fn get_signing_address(&self, algo: &str) -> String {
        match algo.to_lowercase().as_str() {
            "ed25519" => hex::encode(self.ed25519_verifying_key.as_bytes()),
            "ecdsa" => hex::encode(self.ecdsa_verifying_key.to_sec1_bytes()),
            _ => {
                tracing::warn!("Unknown signing algorithm: {}, defaulting to ed25519", algo);
                hex::encode(self.ed25519_verifying_key.as_bytes())
            }
        }
    }

    /// Get the signing address with 0x prefix for the specified algorithm
    pub fn get_signing_address_hex(&self, algo: &str) -> String {
        format!("0x{}", self.get_signing_address(algo))
    }

    /// Get the default signing address (ed25519) for backward compatibility
    pub fn get_default_signing_address(&self) -> String {
        self.get_signing_address("ed25519")
    }

    /// Get the default signing address with 0x prefix (ed25519) for backward compatibility
    pub fn get_default_signing_address_hex(&self) -> String {
        self.get_signing_address_hex("ed25519")
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

    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
        signing_algo: Option<String>,
    ) -> Result<(), AttestationError> {
        // Create signature text in format "request_hash:response_hash"
        let signature_text = format!("{}:{}", request_hash, response_hash);

        // Determine signing algorithm (default to ed25519)
        let algo = signing_algo
            .as_ref()
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ed25519".to_string());

        let (signature_hex, signing_address) = match algo.as_str() {
            "ed25519" => {
                let signature_bytes = self.ed25519_signing_key.sign(signature_text.as_bytes());
                let sig_hex = hex::encode(signature_bytes.to_bytes());
                let addr = self.get_signing_address_hex("ed25519");
                (sig_hex, addr)
            }
            "ecdsa" => {
                // Sign using ECDSA
                let signature: EcdsaSignature =
                    self.ecdsa_signing_key.sign(signature_text.as_bytes());
                let sig_hex = hex::encode(signature.to_bytes());
                let addr = self.get_signing_address_hex("ecdsa");
                (sig_hex, addr)
            }
            _ => {
                tracing::warn!("Unknown signing algorithm: {}, defaulting to ed25519", algo);
                let signature_bytes = self.ed25519_signing_key.sign(signature_text.as_bytes());
                let sig_hex = hex::encode(signature_bytes.to_bytes());
                let addr = self.get_signing_address_hex("ed25519");
                (sig_hex, addr)
            }
        };

        let signing_address_clone = signing_address.clone();
        let algo_clone = algo.clone();

        let signature = ChatSignature {
            text: signature_text.clone(),
            signature: format!("0x{}", signature_hex),
            signing_address,
            signing_algo: algo,
        };

        // Store in repository using response_id as the key
        self.repository
            .add_chat_signature(response_id, signature)
            .await
            .map_err(|e| {
                tracing::error!("Failed to store response signature in repository");
                AttestationError::RepositoryError(e.to_string())
            })?;

        tracing::info!(
            "Stored response signature for response_id: {} with signing_address: {} using algorithm: {}",
            response_id,
            signing_address_clone,
            algo_clone
        );

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

            // Determine which signing algorithm to use (default to ed25519)
            let algo = signing_algo
                .as_ref()
                .map(|s| s.to_lowercase())
                .unwrap_or_else(|| "ed25519".to_string());

            // Use the provided signing_address if given, otherwise use our generated one for the algorithm
            let signing_address_for_provider = signing_address
                .clone()
                .or_else(|| Some(self.get_signing_address_hex(&algo)));

            all_attestations = self
                .inference_provider_pool
                .get_attestation_report(
                    canonical_name.clone(),
                    signing_algo.clone(),
                    Some(nonce.clone()),
                    signing_address_for_provider,
                )
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }

        // Determine which signing algorithm to use for report_data (default to ed25519)
        let algo = signing_algo
            .as_ref()
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ed25519".to_string());

        // Get signing address (public key) for report_data
        // Use the provided signing_address if given, otherwise use our generated one for the algorithm
        // Store in owned String to avoid lifetime issues
        let signing_address_to_use = signing_address
            .clone()
            .unwrap_or_else(|| self.get_signing_address(&algo));

        // Parse nonce: handle hex string or generate if needed
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

        // Parse signing address from hex (remove 0x prefix if present)
        let signing_address_clean = signing_address_to_use
            .strip_prefix("0x")
            .unwrap_or(&signing_address_to_use);
        let signing_address_bytes = hex::decode(signing_address_clean).map_err(|e| {
            tracing::error!("Failed to decode signing address hex string: {}", e);
            AttestationError::InvalidParameter(format!("Invalid signing address format: {e}"))
        })?;

        // For report_data, we need exactly 32 bytes for the signing address
        // ECDSA keys are 33 bytes (compressed) or 65 bytes (uncompressed)
        // We'll take the first 32 bytes for report_data
        let signing_address_for_report = if signing_address_bytes.len() > 32 {
            // Take first 32 bytes (e.g., for ECDSA compressed keys which are 33 bytes)
            signing_address_bytes[..32].to_vec()
        } else {
            signing_address_bytes
        };

        // Build report_data: [signing_address (padded to 32 bytes) || nonce (32 bytes)]
        let mut report_data = vec![0u8; 64];
        // Pad signing address to 32 bytes (left-justified with zeros)
        report_data[..signing_address_for_report.len()]
            .copy_from_slice(&signing_address_for_report);
        // Remaining bytes are already zeros
        // Place nonce in the last 32 bytes
        report_data[32..64].copy_from_slice(&nonce_bytes);

        let gateway_attestation;
        if let Ok(_dev) = std::env::var("DEV") {
            gateway_attestation = DstackCpuQuote {
                intel_quote: "0x1234567890abcdef".to_string(),
                event_log: "0x1234567890abcdef".to_string(),
                report_data: hex::encode(&report_data),
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
            };
        } else {
            let client = dstack_client::DstackClient::new(None);

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
            gateway_attestation = DstackCpuQuote::from_quote_and_nonce(info, cpu_quote, nonce);
        }

        Ok(AttestationReport {
            gateway_attestation,
            all_attestations,
        })
    }
}
