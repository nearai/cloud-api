pub mod models;
use dstack_sdk::dstack_client;
pub use models::{AttestationError, ChatSignature, SignatureLookupResult};
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hex;
use k256::ecdsa::{
    RecoveryId, Signature as EcdsaSignature, SigningKey as EcdsaSigningKey,
    VerifyingKey as EcdsaVerifyingKey,
};
use rand_core::{OsRng, RngCore};
use sha3::{Digest, Keccak256};
use uuid::Uuid;

use crate::{
    attestation::{
        models::{AttestationReport, DstackCpuQuote, VpcInfo},
        ports::AttestationRepository,
    },
    inference_provider_pool::InferenceProviderPool,
    metrics::{consts::*, MetricsServiceTrait},
    models::ModelsRepository,
    usage::{StopReason, UsageRepository},
};

use chrono;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub mod ports;

// Constants for key paths
const GATEWAY_KEY_PATH_ED25519: &str = "/signing-key/ed25519";
const GATEWAY_KEY_PATH_ECDSA: &str = "/signing-key/ecdsa";

pub struct AttestationService {
    pub repository: Arc<dyn AttestationRepository + Send + Sync>,
    pub inference_provider_pool: Arc<InferenceProviderPool>,
    pub models_repository: Arc<dyn ModelsRepository>,
    pub metrics_service: Arc<dyn MetricsServiceTrait>,
    pub usage_repository: Arc<dyn UsageRepository>,
    pub vpc_info: Option<VpcInfo>,
    pub vpc_shared_secret: Option<String>,
    pub tls_cert_fingerprint: Option<String>,
    ed25519_signing_key: Arc<SigningKey>,
    ed25519_verifying_key: Arc<VerifyingKey>,
    ecdsa_signing_key: Arc<EcdsaSigningKey>,
    ecdsa_verifying_key: Arc<EcdsaVerifyingKey>,
}

impl AttestationService {
    pub async fn init(
        repository: Arc<dyn AttestationRepository + Send + Sync>,
        inference_provider_pool: Arc<InferenceProviderPool>,
        models_repository: Arc<dyn ModelsRepository>,
        metrics_service: Arc<dyn MetricsServiceTrait>,
        usage_repository: Arc<dyn UsageRepository>,
    ) -> Result<Self, AttestationError> {
        // Warn if DEV is set in a release build — it has no effect but suggests misconfiguration
        #[cfg(not(debug_assertions))]
        if std::env::var("DEV").is_ok() {
            tracing::error!(
                "SECURITY: DEV environment variable is set in a release build. \
                 DEV mode is not available in release builds and will be ignored. \
                 Remove the DEV variable from your environment."
            );
        }

        // Load VPC info once during initialization
        let vpc_info = load_vpc_info();

        // Load VPC shared secret from environment
        let vpc_shared_secret = load_vpc_shared_secret();

        // Load TLS certificate fingerprint if configured
        let tls_cert_fingerprint = load_tls_cert_fingerprint();
        if vpc_shared_secret.is_none() {
            tracing::warn!(
                "Cannot load VPC shared secret. VPC-based authentication will be disabled"
            );
        }

        // In TEE, use dstack-derived key material based on app_id so the signing address
        // stays stable across multiple instances of the same app.
        // In DEV mode (debug builds only), fall back to per-process ephemeral keys.
        let (ed25519_signing_key, ed25519_verifying_key, ecdsa_signing_key, ecdsa_verifying_key) =
            match Self::derive_signing_keys_from_dstack().await {
                Ok(keys) => keys,
                Err(e) => {
                    // DEV mode fallback is only available in debug builds.
                    // In release builds, dstack key derivation failure is always fatal.
                    #[cfg(debug_assertions)]
                    {
                        if std::env::var("DEV").is_ok() {
                            tracing::warn!(
                                "DEV mode: Unable to derive signing keys from dstack ({}); falling back to ephemeral keys",
                                e
                            );
                            Self::generate_ephemeral_signing_keys()
                        } else {
                            tracing::error!(
                                "Failed to derive signing keys from dstack ({}). \
                                 This service must run in a CVM/TEE with dstack available.",
                                e
                            );
                            return Err(AttestationError::InternalError(format!(
                                "Failed to derive signing keys from dstack: {}. \
                                 Ensure this service runs in a CVM/TEE with dstack available.",
                                e
                            )));
                        }
                    }
                    #[cfg(not(debug_assertions))]
                    {
                        tracing::error!(
                            "Failed to derive signing keys from dstack ({}). \
                             This service must run in a CVM/TEE with dstack available.",
                            e
                        );
                        return Err(AttestationError::InternalError(format!(
                            "Failed to derive signing keys from dstack: {}. \
                             Ensure this service runs in a CVM/TEE with dstack available.",
                            e
                        )));
                    }
                }
            };

        Ok(Self {
            repository,
            inference_provider_pool,
            models_repository,
            metrics_service,
            usage_repository,
            vpc_info,
            vpc_shared_secret,
            tls_cert_fingerprint,
            ed25519_signing_key: Arc::new(ed25519_signing_key),
            ed25519_verifying_key: Arc::new(ed25519_verifying_key),
            ecdsa_signing_key: Arc::new(ecdsa_signing_key),
            ecdsa_verifying_key: Arc::new(ecdsa_verifying_key),
        })
    }

    #[cfg(debug_assertions)]
    fn generate_ephemeral_signing_keys(
    ) -> (SigningKey, VerifyingKey, EcdsaSigningKey, EcdsaVerifyingKey) {
        let mut csprng = OsRng;

        // ed25519 key pair
        let ed25519_signing_key = SigningKey::generate(&mut csprng);
        let ed25519_verifying_key = ed25519_signing_key.verifying_key();
        let ed25519_address = hex::encode(ed25519_verifying_key.as_bytes());
        tracing::info!(
            "Generated ed25519 key pair for response signing (ephemeral). Public key (signing address): {}",
            ed25519_address
        );

        // ECDSA key pair
        let ecdsa_signing_key = EcdsaSigningKey::random(&mut csprng);
        let ecdsa_verifying_key = *ecdsa_signing_key.verifying_key();
        let ecdsa_address_raw = Self::ecdsa_public_key_to_ethereum_address(&ecdsa_verifying_key);
        tracing::info!(
            "Generated ECDSA (secp256k1) key pair for response signing (ephemeral). Ethereum address (signing address): 0x{}",
            hex::encode(ecdsa_address_raw)
        );

        (
            ed25519_signing_key,
            ed25519_verifying_key,
            ecdsa_signing_key,
            ecdsa_verifying_key,
        )
    }

    async fn derive_signing_keys_from_dstack(
    ) -> Result<(SigningKey, VerifyingKey, EcdsaSigningKey, EcdsaVerifyingKey), AttestationError>
    {
        let client = dstack_client::DstackClient::new(None);

        // Get ed25519 signing key from dstack
        // Note: Treating a secp256k1 private key as an ed25519 private key
        // is not theoretically safe, but it is acceptable in practice
        let ed25519_key_resp = client
            .get_key(Some(GATEWAY_KEY_PATH_ED25519.into()), None)
            .await
            .map_err(|e| {
                AttestationError::InternalError(format!(
                    "failed to get ed25519 key from dstack: {e:?}"
                ))
            })?;

        let ed25519_key_bytes = ed25519_key_resp.decode_key().map_err(|e| {
            AttestationError::InternalError(format!("failed to decode ed25519 key hex: {e}"))
        })?;

        // Validate key length (ed25519 requires 32 bytes)
        if ed25519_key_bytes.len() != 32 {
            return Err(AttestationError::InternalError(format!(
                "Invalid ed25519 key length: expected 32 bytes, got {} bytes",
                ed25519_key_bytes.len()
            )));
        }

        let ed25519_key_array: [u8; 32] = ed25519_key_bytes.try_into().map_err(|_| {
            AttestationError::InternalError(
                "Failed to convert ed25519 key bytes to array".to_string(),
            )
        })?;

        let ed25519_signing_key = SigningKey::from_bytes(&ed25519_key_array);
        let ed25519_verifying_key = ed25519_signing_key.verifying_key();

        // Get secp256k1 signing key from dstack
        let ecdsa_key_resp = client
            .get_key(Some(GATEWAY_KEY_PATH_ECDSA.into()), None)
            .await
            .map_err(|e| {
                AttestationError::InternalError(format!(
                    "failed to get ecdsa key from dstack: {e:?}"
                ))
            })?;

        let ecdsa_key_bytes = ecdsa_key_resp.decode_key().map_err(|e| {
            AttestationError::InternalError(format!("failed to decode ecdsa key hex: {e}"))
        })?;

        // Validate key length (secp256k1 requires 32 bytes)
        if ecdsa_key_bytes.len() != 32 {
            return Err(AttestationError::InternalError(format!(
                "Invalid ecdsa key length: expected 32 bytes, got {} bytes",
                ecdsa_key_bytes.len()
            )));
        }

        let ecdsa_key_array: [u8; 32] = ecdsa_key_bytes.try_into().map_err(|_| {
            AttestationError::InternalError(
                "Failed to convert ecdsa key bytes to array".to_string(),
            )
        })?;

        let ecdsa_signing_key =
            EcdsaSigningKey::from_bytes(&ecdsa_key_array.into()).map_err(|_| {
                AttestationError::InternalError("Invalid secp256k1 private key from dstack".into())
            })?;

        let ecdsa_verifying_key = *ecdsa_signing_key.verifying_key();

        let ed25519_address = hex::encode(ed25519_verifying_key.as_bytes());
        tracing::info!(
            "Loaded ed25519 key pair for response signing from dstack. Public key (signing address): {}",
            ed25519_address
        );
        let ecdsa_address_raw = Self::ecdsa_public_key_to_ethereum_address(&ecdsa_verifying_key);
        tracing::info!(
            "Loaded ECDSA (secp256k1) key pair for response signing from dstack. Ethereum address (signing address): 0x{}",
            hex::encode(ecdsa_address_raw)
        );

        Ok((
            ed25519_signing_key,
            ed25519_verifying_key,
            ecdsa_signing_key,
            ecdsa_verifying_key,
        ))
    }

    /// Convert ECDSA public key to Ethereum address (20 bytes)
    /// Ethereum address is derived by: Keccak256(uncompressed_public_key)[12..32]
    fn ecdsa_public_key_to_ethereum_address(verifying_key: &EcdsaVerifyingKey) -> Vec<u8> {
        // Get uncompressed public key point (65 bytes: 0x04 + 32 bytes x + 32 bytes y)
        let encoded_point = verifying_key.to_encoded_point(false);
        let point_bytes = encoded_point.as_bytes();

        // Extract x and y coordinates (skip the 0x04 prefix, take 64 bytes)
        let uncompressed_pubkey = &point_bytes[1..65]; // Skip first byte (0x04), take 64 bytes

        // Hash with Keccak256
        let hash = Keccak256::digest(uncompressed_pubkey);

        // Ethereum address is the last 20 bytes (bytes 12..32)
        let address_bytes = &hash[12..32];

        address_bytes.to_vec()
    }

    /// Get the signing address (public key) as a hex string for the specified algorithm
    /// For ECDSA, returns Ethereum address (20 bytes = 40 hex chars)
    /// For ed25519, returns the public key bytes
    pub fn get_signing_address(&self, algo: &str) -> Result<Vec<u8>, AttestationError> {
        match algo.to_lowercase().as_str() {
            "ed25519" => Ok(self.ed25519_verifying_key.as_bytes().to_vec()),
            "ecdsa" => Ok(Self::ecdsa_public_key_to_ethereum_address(
                &self.ecdsa_verifying_key,
            )),
            signing_algo => Err(AttestationError::InvalidParameter(format!(
                "Unknown signing algorithm: {signing_algo}"
            ))),
        }
    }

    /// Get the signing address hex for the specified algorithm
    pub fn get_signing_address_hex(&self, algo: &str) -> Result<String, AttestationError> {
        match algo.to_lowercase().as_str() {
            "ecdsa" => {
                let addr = self.get_signing_address(algo)?;
                Ok(format!("0x{}", hex::encode(addr)))
            }
            "ed25519" => {
                let addr = self.get_signing_address(algo)?;
                Ok(hex::encode(addr))
            }
            signing_algo => Err(AttestationError::InvalidParameter(format!(
                "Unknown signing algorithm: {signing_algo}"
            ))),
        }
    }

    /// Check if the response/completion was stopped due to client disconnect
    /// If so, return an Unavailable result instead of NotFound error
    async fn check_fallback_conditions(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<SignatureLookupResult, AttestationError> {
        // Query usage repository for stop_reason based on ID format
        let stop_reason = if chat_id.starts_with("resp_") {
            // Response API - query by response_id
            let uuid_str = chat_id.strip_prefix("resp_").unwrap_or(chat_id);
            let response_uuid = match Uuid::parse_str(uuid_str) {
                Ok(uuid) => uuid,
                Err(_) => {
                    return Err(AttestationError::SignatureNotFound(format!(
                        "{}:{}",
                        chat_id, signing_algo
                    )))
                }
            };
            self.usage_repository
                .get_stop_reason_by_response_id(response_uuid)
                .await
                .map_err(|e| AttestationError::RepositoryError(e.to_string()))?
        } else {
            // Chat Completions API - query by provider_request_id
            self.usage_repository
                .get_stop_reason_by_provider_request_id(chat_id)
                .await
                .map_err(|e| AttestationError::RepositoryError(e.to_string()))?
        };

        match stop_reason {
            Some(StopReason::ClientDisconnect) => Ok(SignatureLookupResult::Unavailable {
                error_code: "STREAM_DISCONNECTED".to_string(),
                message: "Verification not available due to disconnection.".to_string(),
            }),
            _ => Err(AttestationError::SignatureNotFound(format!(
                "{}:{}",
                chat_id, signing_algo
            ))),
        }
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

/// Load TLS certificate SPKI fingerprint from the first available cert path.
pub fn load_tls_cert_fingerprint() -> Option<String> {
    let mut failures: Vec<String> = Vec::new();

    fn try_path(path: &str, env_name: &str, failures: &mut Vec<String>) -> Option<String> {
        match compute_spki_hash(path) {
            Ok(hash) => {
                tracing::info!(
                    tls_cert_path = %path,
                    env = env_name,
                    fingerprint = %hash,
                    "TLS certificate SPKI hash computed"
                );
                Some(hash)
            }
            Err(e) => {
                tracing::debug!(
                    tls_cert_path = %path,
                    env = env_name,
                    error = %e,
                    "TLS cert fingerprint attempt failed, trying next candidate if any"
                );
                failures.push(format!("{env_name}={path}: {e}"));
                None
            }
        }
    }

    for env_name in ["INGRESS_TLS_CERT_PATH", "TLS_CERT_PATH"] {
        if let Ok(path) = std::env::var(env_name) {
            if let Some(hash) = try_path(&path, env_name, &mut failures) {
                return Some(hash);
            }
        }
    }

    if !failures.is_empty() {
        tracing::warn!(
            failures = ?failures,
            "Could not compute TLS cert fingerprint from any configured path"
        );
    }
    None
}

/// Compute SHA-256 hash of the Subject Public Key Info (SPKI) DER from a PEM certificate
pub fn compute_spki_hash(cert_path: &str) -> Result<String, String> {
    use sha2::Digest;
    let pem_data =
        std::fs::read(cert_path).map_err(|e| format!("failed to read cert {cert_path}: {e}"))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem_data)
        .map_err(|e| format!("failed to parse PEM: {e}"))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)
        .map_err(|e| format!("failed to parse X.509: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let mut hasher = sha2::Sha256::new();
    hasher.update(spki_der);
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Load VPC shared secret from file
pub fn load_vpc_shared_secret() -> Option<String> {
    if let Ok(path) = std::env::var("VPC_SHARED_SECRET_FILE") {
        std::fs::read_to_string(path)
            .map_err(|_| tracing::warn!("Failed to read VPC shared secret file"))
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    }
}

#[async_trait]
impl ports::AttestationServiceTrait for AttestationService {
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        let signing_algo = signing_algo
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "ecdsa".to_string());

        // Try to get signature from repository
        match self
            .repository
            .get_chat_signature(chat_id, &signing_algo)
            .await
        {
            Ok(signature) => Ok(SignatureLookupResult::Found(signature)),
            Err(AttestationError::SignatureNotFound(_)) => {
                // Check if the response was stopped due to client disconnect
                self.check_fallback_conditions(chat_id, &signing_algo).await
            }
            Err(e) => Err(e),
        }
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

        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");

        // Always fetch and store both ECDSA and ED25519 signatures from the provider/model.
        // This avoids gateway-side signature synthesis and ensures signing_address reflects
        // the provider/model identity.
        for algo in ["ecdsa", "ed25519"] {
            let provider_signature = provider
                .get_signature(chat_id, Some(algo.to_string()))
                .await
                .map_err(|e| {
                    tracing::error!(
                        "Failed to get chat signature from provider for algorithm: {}",
                        algo
                    );
                    let duration = start_time.elapsed();
                    self.metrics_service.record_count(
                        METRIC_VERIFICATION_FAILURE,
                        1,
                        &[&format!("{TAG_REASON}:{REASON_INFERENCE_ERROR}"), &env_tag],
                    );
                    self.metrics_service.record_latency(
                        METRIC_VERIFICATION_DURATION,
                        duration,
                        &[&env_tag],
                    );
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
                    tracing::error!(
                        "Failed to store chat signature in repository for algorithm: {}",
                        algo
                    );
                    let duration = start_time.elapsed();
                    self.metrics_service.record_count(
                        METRIC_VERIFICATION_FAILURE,
                        1,
                        &[&format!("{TAG_REASON}:{REASON_REPOSITORY_ERROR}"), &env_tag],
                    );
                    self.metrics_service.record_latency(
                        METRIC_VERIFICATION_DURATION,
                        duration,
                        &[&env_tag],
                    );
                    AttestationError::RepositoryError(e.to_string())
                })?;
        }

        // Record successful verification
        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[&env_tag]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[&env_tag]);

        Ok(())
    }

    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        let start_time = std::time::Instant::now();
        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");

        // Create signature text in format "request_hash:response_hash"
        let signature_text = format!("{request_hash}:{response_hash}");

        // Generate and store both ECDSA and ED25519 signatures
        for algo in ["ecdsa", "ed25519"] {
            let (signature_hex, signing_address) = match algo {
                "ed25519" => {
                    let signature_bytes = self.ed25519_signing_key.sign(signature_text.as_bytes());
                    let sig_hex = hex::encode(signature_bytes.to_bytes());
                    let addr = self.get_signing_address_hex("ed25519")?;
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

                    let addr = self.get_signing_address_hex("ecdsa")?;
                    Ok((format!("0x{sig_hex}"), addr))
                }
                _ => Err(AttestationError::InvalidParameter(format!(
                    "Unknown signing algorithm: {algo}"
                ))),
            }?;

            let signature = ChatSignature {
                text: signature_text.clone(),
                signature: signature_hex,
                signing_address,
                signing_algo: algo.to_string(),
            };

            // Store in repository using response_id as the key
            self.repository
                .add_chat_signature(response_id, signature)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "Failed to store response signature in repository for algorithm: {}",
                        algo
                    );
                    AttestationError::RepositoryError(e.to_string())
                })?;

            tracing::info!(
                "Stored response signature for response_id: {} with algorithm: {}",
                response_id,
                algo
            );
        }

        // Record successful verification
        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[&env_tag]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[&env_tag]);

        Ok(())
    }

    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<AttestationReport, AttestationError> {
        // Resolve model name (could be an alias) and get model details
        let mut model_attestations = vec![];
        // Create a nonce if none was provided
        let nonce = nonce.unwrap_or_else(|| {
            let mut nonce_bytes = [0u8; 32];
            OsRng.fill_bytes(&mut nonce_bytes);
            let generated_nonce = nonce_bytes
                .into_iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            tracing::debug!(
                "No nonce provided for attestation report, generated nonce: {}",
                generated_nonce
            );
            generated_nonce
        });

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
                    include_tls_fingerprint,
                )
                .await
                .map_err(|e| AttestationError::ProviderError(e.to_string()))?;
        }

        // Use VPC info loaded at initialization
        let vpc = self.vpc_info.clone();

        // Get signing address (public key) for report_data
        // Store in owned String to avoid lifetime issues
        let signing_address_to_use = self.get_signing_address_hex(&algo)?;

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

        // Resolve TLS cert fingerprint if requested
        let tls_fingerprint = if include_tls_fingerprint {
            Some(self.tls_cert_fingerprint.clone().ok_or_else(|| {
                AttestationError::InternalError(
                    "include_tls_fingerprint=true but neither INGRESS_TLS_CERT_PATH nor TLS_CERT_PATH is set or fingerprint could not be computed".to_string(),
                )
            })?)
        } else {
            None
        };

        // Read TLS certificate PEM if fingerprint is requested (for the response body)
        let tls_certificate = if include_tls_fingerprint {
            if let Ok(path) = std::env::var("INGRESS_TLS_CERT_PATH") {
                tokio::fs::read_to_string(&path).await.ok()
            } else if let Ok(path) = std::env::var("TLS_CERT_PATH") {
                tokio::fs::read_to_string(&path).await.ok()
            } else {
                None
            }
        } else {
            None
        };

        // Build report_data: [first_32_bytes || nonce (32 bytes)]
        // When include_tls_fingerprint=true: first_32 = SHA256(signing_address_bytes || cert_fingerprint_bytes)
        // Otherwise: first_32 = signing_address (right-padded with zeros)
        let mut report_data = vec![0u8; 64];
        if let Some(ref fp_hex) = tls_fingerprint {
            use sha2::Digest;
            let fp_bytes = hex::decode(fp_hex).map_err(|e| {
                AttestationError::InternalError(format!("bad cert fingerprint hex: {e}"))
            })?;
            let mut hasher = sha2::Sha256::new();
            hasher.update(&signing_address_for_report);
            hasher.update(&fp_bytes);
            let hash = hasher.finalize();
            report_data[..32].copy_from_slice(&hash);
        } else {
            report_data[..signing_address_for_report.len()]
                .copy_from_slice(&signing_address_for_report);
        }
        report_data[32..64].copy_from_slice(&nonce_bytes);

        // Fake attestation data is only available in debug builds with DEV set.
        // In release builds, real dstack attestation is always required.
        // Both the fake data branch and real dstack branch are gated with #[cfg] so
        // fake attestation strings are physically absent from release binaries.
        let gateway_attestation;
        #[cfg(debug_assertions)]
        {
            if std::env::var("DEV").is_ok() {
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
                        "os_image_hash": "dev-os-image-id",
                        "key_provider_info": "dev-key-provider-info",
                        "compose_hash": "dev-compose-hash",
                        "vm_config": {},
                    }),
                    vpc,
                    tls_cert_fingerprint: tls_fingerprint.clone(),
                };
            } else {
                let client = dstack_client::DstackClient::new(None);

                let info = client.info().await.map_err(|e| {
                    tracing::error!(
                        "Failed to get cloud API attestation info, are you running in a CVM?: {e:?}"
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
                    AttestationError::InternalError(
                        "failed to get cloud API attestation".to_string(),
                    )
                })?;
                gateway_attestation = DstackCpuQuote::from_quote_and_nonce(
                    signing_address_to_use,
                    algo,
                    vpc,
                    info,
                    cpu_quote,
                    nonce,
                    tls_fingerprint.clone(),
                );
            }
        }
        #[cfg(not(debug_assertions))]
        {
            let client = dstack_client::DstackClient::new(None);

            let info = client.info().await.map_err(|e| {
                tracing::error!(
                    "Failed to get cloud API attestation info, are you running in a CVM?: {e:?}"
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
                tls_fingerprint,
            );
        }

        Ok(AttestationReport {
            gateway_attestation,
            model_attestations,
            tls_certificate,
        })
    }

    async fn verify_vpc_signature(
        &self,
        timestamp: i64,
        signature: String,
    ) -> Result<bool, AttestationError> {
        let secret = self.vpc_shared_secret.as_ref().ok_or_else(|| {
            AttestationError::InternalError("Failed to load VPC shared secret".to_string())
        })?;

        // Check timestamp freshness (within 30 seconds)
        let now = chrono::Utc::now().timestamp();
        let diff = (now - timestamp).abs();
        if diff > 30 {
            tracing::warn!(
                "VPC signature timestamp expired: current={now}, provided={timestamp}, diff={diff}"
            );
            return Ok(false);
        }

        // Decode provided signature from hex
        let provided_bytes = match hex::decode(&signature) {
            Ok(bytes) => bytes,
            Err(_) => {
                tracing::warn!("Invalid hex in VPC signature");
                return Ok(false);
            }
        };

        // Verify signature: HMAC-SHA256(timestamp, secret)
        let message = timestamp.to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|e| AttestationError::InternalError(format!("Failed to create HMAC: {e}")))?;
        mac.update(message.as_bytes());

        // Constant-time comparison
        match mac.verify_slice(&provided_bytes) {
            Ok(_) => Ok(true),
            Err(_) => {
                tracing::warn!("VPC signature mismatch");
                Ok(false)
            }
        }
    }
}
