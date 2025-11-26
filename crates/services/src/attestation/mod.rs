pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature};
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hex;
use k256::ecdsa::{
    RecoveryId, Signature as EcdsaSignature, SigningKey as EcdsaSigningKey,
    VerifyingKey as EcdsaVerifyingKey,
};
use rand::rngs::OsRng;
use rand::RngCore;
use sha3::{Digest, Keccak256};

use crate::{
    attestation::{
        models::{AttestationReport, DstackCpuQuote, VpcInfo},
        ports::AttestationRepository,
    },
    inference_provider_pool::InferenceProviderPool,
    metrics::{consts::*, MetricsServiceTrait},
    models::ModelsRepository,
};

pub mod ports;

pub struct AttestationService {
    pub repository: Arc<dyn AttestationRepository + Send + Sync>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
    pub vpc_info: Option<VpcInfo>,
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
        metrics_service: Arc<dyn MetricsServiceTrait>,
    ) -> Self {
        // Load VPC info once during initialization
        let vpc_info = load_vpc_info();
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

        // Convert ECDSA public key to Ethereum address (20 bytes = 40 hex chars)
        let ecdsa_address = Self::ecdsa_public_key_to_ethereum_address(&ecdsa_verifying_key);
        tracing::info!(
            "Generated ECDSA (secp256k1) key pair for response signing. Ethereum address (signing address): 0x{}",
            ecdsa_address
        );

        Self {
            repository,
            inference_provider_pool,
            models_repository,
            metrics_service,
            vpc_info,
            ed25519_signing_key,
            ed25519_verifying_key,
            ecdsa_signing_key,
            ecdsa_verifying_key,
        }
    }

    /// Convert ECDSA public key to Ethereum address (20 bytes)
    /// Ethereum address is derived by: Keccak256(uncompressed_public_key)[12..32]
    fn ecdsa_public_key_to_ethereum_address(verifying_key: &EcdsaVerifyingKey) -> String {
        // Get uncompressed public key point (65 bytes: 0x04 + 32 bytes x + 32 bytes y)
        let encoded_point = verifying_key.to_encoded_point(false);
        let point_bytes = encoded_point.as_bytes();

        // Extract x and y coordinates (skip the 0x04 prefix, take 64 bytes)
        let uncompressed_pubkey = &point_bytes[1..65]; // Skip first byte (0x04), take 64 bytes

        // Hash with Keccak256
        let hash = Keccak256::digest(uncompressed_pubkey);

        // Ethereum address is the last 20 bytes (bytes 12..32)
        let address_bytes = &hash[12..32];

        hex::encode(address_bytes)
    }

    /// Get the signing address (public key) as a hex string for the specified algorithm
    /// For ECDSA, returns Ethereum address (20 bytes = 40 hex chars)
    /// For ed25519, returns the public key bytes
    pub fn get_signing_address(&self, algo: &str) -> String {
        match algo.to_lowercase().as_str() {
            "ed25519" => hex::encode(self.ed25519_verifying_key.as_bytes()),
            "ecdsa" => Self::ecdsa_public_key_to_ethereum_address(&self.ecdsa_verifying_key),
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

/// Load VPC (Virtual Private Cloud) information from environment variables
pub fn load_vpc_info() -> Option<VpcInfo> {
    // Read VPC server app ID from environment
    let vpc_server_app_id = std::env::var("VPC_SERVER_APP_ID").ok();

    // Read VPC hostname from file
    let vpc_hostname = if let Ok(path) = std::env::var("VPC_HOSTNAME_FILE") {
        std::fs::read_to_string(path)
            .map_err(|e| tracing::warn!("Failed to read VPC hostname file: {e}"))
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
        let start_time = std::time::Instant::now();

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
            let duration = start_time.elapsed();
            self.metrics_service.record_count(
                METRIC_VERIFICATION_FAILURE,
                1,
                &[&format!("{TAG_REASON}:{REASON_INFERENCE_ERROR}")],
            );
            self.metrics_service
                .record_latency(METRIC_VERIFICATION_DURATION, duration, &[]);
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
                let duration = start_time.elapsed();
                self.metrics_service.record_count(
                    METRIC_VERIFICATION_FAILURE,
                    1,
                    &[&format!("{TAG_REASON}:{REASON_REPOSITORY_ERROR}")],
                );
                self.metrics_service
                    .record_latency(METRIC_VERIFICATION_DURATION, duration, &[]);
                AttestationError::RepositoryError(e.to_string())
            })?;

        // Record successful verification
        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[]);

        Ok(())
    }

    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
        signing_algo: Option<String>,
    ) -> Result<(), AttestationError> {
        let start_time = std::time::Instant::now();

        // Create signature text in format "request_hash:response_hash"
        let signature_text = format!("{request_hash}:{response_hash}");

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
                Ok((sig_hex, addr))
            }
            "ecdsa" => {
                // Sign using ECDSA with recovery ID
                // Use Ethereum signed message format
                let message_bytes = signature_text.as_bytes();
                let prefix = format!("\x19Ethereum Signed Message:\n{}", message_bytes.len());
                let prefix_bytes = prefix.as_bytes();

                // Concatenate prefix + message
                let mut prefixed_message =
                    Vec::with_capacity(prefix_bytes.len() + message_bytes.len());
                prefixed_message.extend_from_slice(prefix_bytes);
                prefixed_message.extend_from_slice(message_bytes);

                // Hash with Keccak256 (manually hash the prefixed message)
                let mut hasher = Keccak256::new();
                hasher.update(&prefixed_message);
                let message_hash = hasher.finalize();

                // Use sign_prehash_recoverable with the pre-hashed message
                let (signature, recid): (EcdsaSignature, RecoveryId) = self
                    .ecdsa_signing_key
                    .sign_prehash_recoverable(&message_hash)
                    .map_err(|e| {
                        tracing::error!("Failed to create recoverable ECDSA signature: {}", e);
                        AttestationError::InternalError(format!(
                            "Failed to create recoverable ECDSA signature: {e}"
                        ))
                    })?;

                // Convert signature to bytes and append recovery ID
                // Convert k256 RecoveryId (0-3) to Ethereum v format (27-28)
                // Ethereum v = 27 + (recovery_id & 1) where bit 0 is the y-coordinate parity
                let recovery_byte = recid.to_byte();
                let ethereum_v = 27u8 + (recovery_byte & 1);

                // This creates a 65-byte signature (64 bytes r||s + 1 byte Ethereum v)
                let mut signature_bytes = signature.to_bytes().to_vec();
                signature_bytes.push(ethereum_v);
                let sig_hex = hex::encode(signature_bytes);

                let addr = self.get_signing_address_hex("ecdsa");
                Ok((sig_hex, addr))
            }
            _ => {
                tracing::warn!("Unknown signing algorithm: {}, defaulting to ed25519", algo);
                let signature_bytes = self.ed25519_signing_key.sign(signature_text.as_bytes());
                let sig_hex = hex::encode(signature_bytes.to_bytes());
                let addr = self.get_signing_address_hex("ed25519");
                Ok((sig_hex, addr))
            }
        }?;

        let signing_address_clone = signing_address.clone();
        let algo_clone = algo.clone();

        let signature = ChatSignature {
            text: signature_text.clone(),
            signature: format!("0x{signature_hex}"),
            signing_address,
            signing_algo: algo,
        };

        // Store in repository using response_id as the key
        self.repository
            .add_chat_signature(response_id, signature)
            .await
            .map_err(|e| {
                tracing::error!("Failed to store response signature in repository");
                let duration = start_time.elapsed();
                self.metrics_service.record_count(
                    METRIC_VERIFICATION_FAILURE,
                    1,
                    &[&format!("{TAG_REASON}:{REASON_REPOSITORY_ERROR}")],
                );
                self.metrics_service
                    .record_latency(METRIC_VERIFICATION_DURATION, duration, &[]);
                AttestationError::RepositoryError(e.to_string())
            })?;

        tracing::info!(
            "Stored response signature for response_id: {} with signing_address: {} using algorithm: {}",
            response_id,
            signing_address_clone,
            algo_clone
        );

        // Record successful verification
        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[]);

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
        let mut model_attestations = vec![];
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

        // Determine which signing algorithm to use for report_data (default to ed25519)
        let algo = signing_algo
            .as_ref()
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ed25519".to_string());

        if algo != "ecdsa" && algo != "ed25519" {
            return Err(AttestationError::InvalidParameter(format!(
                "Invalid signing algorithm: {algo}, must be 'ecdsa' or 'ed25519'"
            )));
        }

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

            model_attestations = self
                .inference_provider_pool
                .get_attestation_report(
                    canonical_name.clone(),
                    signing_algo.clone(),
                    Some(nonce.clone()),
                    signing_address,
                )
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }

        // Use VPC info loaded at initialization
        let vpc = self.vpc_info.clone();

        // Get signing address (public key) for report_data
        // Store in owned String to avoid lifetime issues
        let signing_address_to_use = self.get_signing_address_hex(&algo);

        // Parse signing address from hex (remove 0x prefix if present)
        let signing_address_clean = signing_address_to_use
            .strip_prefix("0x")
            .unwrap_or(&signing_address_to_use);
        let signing_address_bytes = hex::decode(signing_address_clean).map_err(|e| {
            tracing::error!("Failed to decode signing address hex string: {}", e);
            AttestationError::InvalidParameter(format!("Invalid signing address format: {e}"))
        })?;

        // For report_data, we need exactly 32 bytes for the signing address
        // ECDSA returns Ethereum address (20 bytes), ed25519 returns public key (32 bytes)
        // We'll pad to 32 bytes if needed (left-justified with zeros)
        let signing_address_for_report = if signing_address_bytes.len() > 32 {
            // Take first 32 bytes if longer (shouldn't happen with current implementation)
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
                signing_address: signing_address_to_use,
                signing_algo: algo,
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
                vpc,
            };
        } else {
            let client = dstack_client::DstackClient::new(None);

            let info = client.info().await.map_err(|e| {
                tracing::error!(
                    "Failed to get cloud API attestation info, are you running in a CVM?: {:?}",
                    e
                );
                AttestationError::InternalError(
                    "failed to get cloud API attestation info".to_string(),
                )
            })?;

            let cpu_quote = client.get_quote(report_data).await.map_err(|e| {
                tracing::error!(
                    "Failed to get cloud API attestation, are you running in a CVM?: {:?}",
                    e
                );
                AttestationError::InternalError("failed to get cloud API attestation".to_string())
            })?;
            gateway_attestation = DstackCpuQuote::from_quote_and_nonce(
                signing_address_to_use,
                algo,
                vpc,
                info,
                cpu_quote,
                nonce,
            );
        }

        Ok(AttestationReport {
            gateway_attestation,
            model_attestations,
        })
    }
}
