pub mod chutes;
pub mod measurement;
pub mod models;
pub mod report_data;
pub mod verification;
use dstack_sdk::dstack_client;
pub use measurement::MeasurementPolicy;
pub use models::{AttestationError, ChatSignature, SignatureLookupResult};
pub use report_data::{ReportDataVerifier, StrictBoundReportDataVerifier};
use std::sync::Arc;
pub use verification::{AttestationVerificationError, AttestationVerifier, VerifiedAttestation};

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
use hmac::{Hmac, KeyInit, Mac};
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

    /// Returns the raw Ed25519 private key seed for OHTTP key derivation.
    ///
    /// The OHTTP gateway derives its HPKE keypair from this seed via `KeyConfig::derive`,
    /// which uses a different derivation path than the E2EE X25519 key — domain-separated.
    pub fn ed25519_secret_bytes(&self) -> [u8; 32] {
        self.ed25519_signing_key.to_bytes()
    }

    /// Signs `data` with the Ed25519 key; returns `(hex_signature, hex_public_key)`.
    ///
    /// Used to produce the `ohttp_attestation` payload: clients can verify the OHTTP
    /// key config bytes are signed by the attested TEE Ed25519 key.
    pub fn sign_ohttp_attestation(&self, data: &[u8]) -> (String, String) {
        let sig = self.ed25519_signing_key.sign(data);
        let signature = hex::encode(sig.to_bytes());
        let signing_key_hex = hex::encode(self.ed25519_verifying_key.as_bytes());
        (signature, signing_key_hex)
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

    async fn store_gateway_signature(
        &self,
        signature_id: &str,
        id_label: &str,
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
                    // Sign using ECDSA with recovery ID and Ethereum signed message format.
                    let message_bytes = signature_text.as_bytes();
                    let prefix = format!("\x19Ethereum Signed Message:\n{}", message_bytes.len());
                    let prefix_bytes = prefix.as_bytes();

                    let mut prefixed_message =
                        Vec::with_capacity(prefix_bytes.len() + message_bytes.len());
                    prefixed_message.extend_from_slice(prefix_bytes);
                    prefixed_message.extend_from_slice(message_bytes);

                    let mut hasher = Keccak256::new();
                    hasher.update(&prefixed_message);
                    let message_hash = hasher.finalize();

                    let (signature, recid): (EcdsaSignature, RecoveryId) = self
                        .ecdsa_signing_key
                        .sign_prehash_recoverable(&message_hash)
                        .map_err(|e| {
                            tracing::error!("Failed to create recoverable ECDSA signature: {}", e);
                            AttestationError::InternalError(format!(
                                "Failed to create recoverable ECDSA signature: {e}"
                            ))
                        })?;

                    let recovery_byte = recid.to_byte();
                    let ethereum_v = 27u8 + (recovery_byte & 1);

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

            self.repository
                .add_chat_signature(signature_id, signature)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "Failed to store {} signature in repository for algorithm: {}",
                        id_label,
                        algo
                    );
                    AttestationError::RepositoryError(e.to_string())
                })?;

            tracing::info!(
                signature_kind = id_label,
                signature_id = signature_id,
                signing_algo = algo,
                "Stored gateway signature"
            );
        }

        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_SIGNATURE_CREATION_SUCCESS, 1, &[&env_tag]);
        self.metrics_service.record_latency(
            METRIC_SIGNATURE_CREATION_DURATION,
            duration,
            &[&env_tag],
        );

        Ok(())
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

/// Load TLS certificate SPKI fingerprint from `TLS_CERT_PATH`.
pub fn load_tls_cert_fingerprint() -> Option<String> {
    let path = std::env::var("TLS_CERT_PATH").ok()?;
    match compute_spki_hash(&path) {
        Ok(hash) => {
            tracing::info!(
                tls_cert_path = %path,
                fingerprint = %hash,
                "TLS certificate SPKI hash computed"
            );
            Some(hash)
        }
        Err(e) => {
            tracing::warn!(
                tls_cert_path = %path,
                error = %e,
                "Failed to compute TLS cert fingerprint (TLS_CERT_PATH)"
            );
            None
        }
    }
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
                // Some attested providers (e.g. Chutes) have no per-response
                // signature. Report that explicitly only after checking the
                // repository, because rewritten public streams may store a
                // gateway signature even when the provider itself cannot sign.
                if let Some(provider) = self
                    .inference_provider_pool
                    .get_provider_by_chat_id(chat_id)
                    .await
                {
                    if !provider.supports_chat_signatures() {
                        return Ok(SignatureLookupResult::Unavailable {
                            error_code: "SIGNATURE_UNSUPPORTED".to_string(),
                            message: "This model's responses are integrity-protected by an \
                                      end-to-end encrypted channel, not a per-response signature; \
                                      there is no signature to retrieve."
                                .to_string(),
                        });
                    }
                }
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

        // Some attested providers don't expose per-response signatures — e.g.
        // Chutes, whose integrity is the ML-KEM E2EE channel's AEAD tag, not a
        // signed response. For those, there's nothing to fetch/store, so skip the
        // signature path entirely (calling get_signature would just error and add
        // failure-metric noise on every attested completion).
        if !provider.supports_chat_signatures() {
            return Ok(());
        }

        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");

        // Always fetch and store both ECDSA and ED25519 signatures from the provider/model.
        // This avoids gateway-side signature synthesis and ensures signing_address reflects
        // the provider/model identity.
        //
        // Use a closure to ensure unpin_chat_connection runs on all paths (success or error),
        // preventing the dedicated client from leaking in signature_clients.
        let result: Result<(), AttestationError> = async {
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
            Ok(())
        }
        .await;

        // Always clean up the dedicated TLS connection, even on error
        provider.unpin_chat_connection(chat_id);

        result?;

        // Record successful verification
        let duration = start_time.elapsed();
        self.metrics_service
            .record_count(METRIC_VERIFICATION_SUCCESS, 1, &[&env_tag]);
        self.metrics_service
            .record_latency(METRIC_VERIFICATION_DURATION, duration, &[&env_tag]);

        Ok(())
    }

    async fn store_chat_signature(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(chat_id, "chat", request_hash, response_hash)
            .await
    }

    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_gateway_signature(response_id, "response", request_hash, response_hash)
            .await
    }

    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
    ) -> Result<AttestationReport, AttestationError> {
        // Track whether the caller supplied a nonce. Only caller-supplied nonces are
        // forwarded to inference-proxy: when None, inference-proxy can serve its
        // 5-minute cached report (skipping a fresh GPU-evidence NVIDIA NRAS round-trip
        // that serializes behind a per-backend Mutex and costs ~700 ms per call).
        let user_provided_nonce = nonce.clone();

        // Produce (gateway_nonce: String, nonce_bytes: Vec<u8>) in one pass:
        // – caller-supplied nonce: validate hex, decode once.
        // – auto-generated nonce: fill random bytes, hex-encode once (no round-trip).
        let (gateway_nonce, nonce_bytes) = match nonce {
            Some(n) => {
                let bytes = hex::decode(&n).map_err(|e| {
                    tracing::error!("Failed to decode nonce hex string: {}", e);
                    AttestationError::InvalidParameter(format!("Invalid nonce format: {e}"))
                })?;
                if bytes.len() != 32 {
                    return Err(AttestationError::InvalidParameter(format!(
                        "Nonce must be exactly 32 bytes, got {} bytes",
                        bytes.len()
                    )));
                }
                (n, bytes)
            }
            None => {
                let mut bytes = vec![0u8; 32];
                OsRng.fill_bytes(&mut bytes);
                let hex = hex::encode(&bytes);
                tracing::debug!("No nonce provided for attestation report, generated nonce: {hex}");
                (hex, bytes)
            }
        };

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

        // Resolve model alias synchronously (fast DB lookup ~10 ms) before spawning
        // the parallel futures below.
        let resolved_canonical = if let Some(ref m) = model {
            let resolved_model = self
                .models_repository
                .resolve_and_get_model(m)
                .await
                .map_err(|e| {
                    AttestationError::ProviderError(format!("Failed to resolve model: {e}"))
                })?
                .ok_or_else(|| {
                    AttestationError::ProviderError(format!(
                        "Model '{m}' not found. It's not a valid model name or alias."
                    ))
                })?;
            let canonical = resolved_model.model_name.clone();
            if &canonical != m {
                tracing::debug!(
                    requested_model = %m,
                    canonical_model = %canonical,
                    "Resolved alias to canonical model name for attestation report"
                );
            }
            Some(canonical)
        } else {
            None
        };

        // Prepare gateway-quote inputs synchronously (no await needed).
        let vpc = self.vpc_info.clone();
        let signing_address_to_use = self.get_signing_address_hex(&algo)?;
        let signing_address_clean = signing_address_to_use
            .strip_prefix("0x")
            .unwrap_or(&signing_address_to_use);
        let signing_address_bytes = hex::decode(signing_address_clean).map_err(|e| {
            tracing::error!("Failed to decode signing address hex string: {}", e);
            AttestationError::InvalidParameter(format!("Invalid signing address format: {e}"))
        })?;
        let signing_address_for_report = if signing_address_bytes.len() > 32 {
            signing_address_bytes[..32].to_vec()
        } else {
            signing_address_bytes
        };

        let tls_fingerprint = if include_tls_fingerprint {
            Some(self.tls_cert_fingerprint.clone().ok_or_else(|| {
                AttestationError::InternalError(
                    "include_tls_fingerprint=true but TLS_CERT_PATH is not set or fingerprint could not be computed".to_string(),
                )
            })?)
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

        // Read TLS certificate PEM if fingerprint is requested (for the response body).
        let tls_certificate = if include_tls_fingerprint {
            if let Ok(path) = std::env::var("TLS_CERT_PATH") {
                tokio::fs::read_to_string(&path).await.ok()
            } else {
                None
            }
        } else {
            None
        };

        // Run model-attestation fetch and gateway TDX-quote generation concurrently.
        // These are independent: the model fetch is an outbound HTTP call to the
        // inference backend; the gateway quote is a local dstack Unix-socket call.
        let model_fut = {
            let pool = &self.inference_provider_pool;
            async move {
                if let Some(canonical) = resolved_canonical {
                    pool.get_attestation_report(
                        canonical,
                        signing_algo,
                        // Key fix: only forward the nonce when the caller supplied one.
                        // When None, inference-proxy serves its 5-min cached report
                        // instead of forcing a fresh GPU-evidence collection (~700 ms).
                        user_provided_nonce,
                        signing_address,
                        include_tls_fingerprint,
                    )
                    .await
                    .map_err(|e| AttestationError::ProviderError(e.to_string()))
                } else {
                    Ok(vec![])
                }
            }
        };

        let gateway_fut = build_gateway_quote(
            signing_address_to_use,
            algo,
            vpc,
            report_data,
            gateway_nonce,
            tls_fingerprint,
        );

        let (model_attestations, gateway_attestation) = tokio::try_join!(model_fut, gateway_fut)?;

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

/// Generate the gateway CVM's TDX quote for an attestation report.
///
/// Fake attestation strings are only available in debug builds with `DEV` set —
/// they are physically absent from release binaries (the early-return block is
/// gated with `#[cfg(debug_assertions)]`).
async fn build_gateway_quote(
    signing_address: String,
    algo: String,
    vpc: Option<VpcInfo>,
    report_data: Vec<u8>,
    nonce: String,
    tls_fingerprint: Option<String>,
) -> Result<DstackCpuQuote, AttestationError> {
    #[cfg(debug_assertions)]
    if std::env::var("DEV").is_ok() {
        return Ok(DstackCpuQuote {
            signing_address,
            signing_algo: algo,
            intel_quote: "0x1234567890abcdef".to_string(),
            event_log: "0x1234567890abcdef".to_string(),
            report_data: hex::encode(&report_data),
            request_nonce: nonce,
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
            tls_cert_fingerprint: tls_fingerprint,
        });
    }

    let client = dstack_client::DstackClient::new(None);

    let info = client.info().await.map_err(|e| {
        tracing::error!(
            "Failed to get cloud API attestation info, are you running in a CVM?: {e:?}"
        );
        AttestationError::InternalError("failed to get cloud API attestation info".to_string())
    })?;

    let cpu_quote = client.get_quote(report_data).await.map_err(|e| {
        tracing::error!(
            "Failed to get cloud API attestation, are you running in a CVM?: {:?}",
            e
        );
        AttestationError::InternalError("failed to get cloud API attestation".to_string())
    })?;

    Ok(DstackCpuQuote::from_quote_and_nonce(
        signing_address,
        algo,
        vpc,
        info,
        cpu_quote,
        nonce,
        tls_fingerprint,
    ))
}
