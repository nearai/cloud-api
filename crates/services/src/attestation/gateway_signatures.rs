use ed25519_dalek::Signer;
use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature};
use sha3::{Digest, Keccak256};

use super::{AttestationError, AttestationService, ChatSignature, SignatureKind};
use crate::metrics::consts::*;

impl AttestationService {
    pub(in crate::attestation) async fn store_gateway_signature(
        &self,
        signature_id: &str,
        id_label: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        let start_time = std::time::Instant::now();
        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");
        let signature_text = format!("{request_hash}:{response_hash}");

        for algo in ["ecdsa", "ed25519"] {
            let (signature_hex, signing_address) = match algo {
                "ed25519" => self.sign_ed25519_gateway_signature(&signature_text),
                "ecdsa" => self.sign_ecdsa_gateway_signature(&signature_text),
                _ => Err(AttestationError::InvalidParameter(format!(
                    "Unknown signing algorithm: {algo}"
                ))),
            }?;

            self.repository
                .add_chat_signature(
                    signature_id,
                    ChatSignature {
                        text: signature_text.clone(),
                        signature: signature_hex,
                        signing_address,
                        signing_algo: algo.to_string(),
                        signature_kind: Some(SignatureKind::Gateway),
                    },
                )
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

    fn sign_ed25519_gateway_signature(
        &self,
        signature_text: &str,
    ) -> Result<(String, String), AttestationError> {
        let signature_bytes = self.ed25519_signing_key.sign(signature_text.as_bytes());
        let sig_hex = hex::encode(signature_bytes.to_bytes());
        let addr = self.get_signing_address_hex("ed25519")?;
        Ok((sig_hex, addr))
    }

    fn sign_ecdsa_gateway_signature(
        &self,
        signature_text: &str,
    ) -> Result<(String, String), AttestationError> {
        let message_bytes = signature_text.as_bytes();
        let prefix = format!("\x19Ethereum Signed Message:\n{}", message_bytes.len());
        let mut prefixed_message = Vec::with_capacity(prefix.len() + message_bytes.len());
        prefixed_message.extend_from_slice(prefix.as_bytes());
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

        let mut signature_bytes = signature.to_bytes().to_vec();
        signature_bytes.push(27u8 + (recid.to_byte() & 1));
        let addr = self.get_signing_address_hex("ecdsa")?;
        Ok((format!("0x{}", hex::encode(signature_bytes)), addr))
    }
}
