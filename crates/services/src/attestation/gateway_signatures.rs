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
        if signature_id.is_empty() {
            return Err(AttestationError::InvalidParameter(
                "Signature ID must not be empty".to_string(),
            ));
        }
        let start_time = std::time::Instant::now();
        let environment = get_environment();
        let env_tag = format!("{TAG_ENVIRONMENT}:{environment}");
        let signature_text = format!("{request_hash}:{response_hash}");

        let mut signatures = Vec::with_capacity(2);
        for algo in ["ecdsa", "ed25519"] {
            let (signature_hex, signing_address) = match algo {
                "ed25519" => self.sign_ed25519_gateway_signature(&signature_text),
                "ecdsa" => self.sign_ecdsa_gateway_signature(&signature_text),
                _ => Err(AttestationError::InvalidParameter(format!(
                    "Unknown signing algorithm: {algo}"
                ))),
            }?;
            signatures.push(ChatSignature {
                text: signature_text.clone(),
                signature: signature_hex,
                signing_address,
                signing_algo: algo.to_string(),
                signature_kind: Some(SignatureKind::Gateway),
            });
        }

        self.repository
            .add_chat_signatures(signature_id, signatures)
            .await
            .map_err(|e| {
                tracing::error!(
                    signature_kind = id_label,
                    signature_id = signature_id,
                    error = %e,
                    "Failed to store gateway signatures in repository"
                );
                AttestationError::RepositoryError(e.to_string())
            })?;
        tracing::info!(
            signature_kind = id_label,
            signature_id = signature_id,
            "Stored gateway signatures"
        );

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
            .sign_prehash_recoverable(&message_hash);

        let mut signature_bytes = signature.to_bytes().to_vec();
        signature_bytes.push(27u8 + (recid.to_byte() & 1));
        let addr = self.get_signing_address_hex("ecdsa")?;
        Ok((format!("0x{}", hex::encode(signature_bytes)), addr))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use config::ExternalProvidersConfig;
    use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

    use super::*;
    use crate::attestation::chat_signature_lifecycle_tests::{
        lifecycle_service, RecordingRepository,
    };
    use crate::inference_provider_pool::InferenceProviderPool;

    #[test]
    fn ecdsa_gateway_signature_preserves_legacy_wire_format() {
        let pool = Arc::new(InferenceProviderPool::new(
            None,
            ExternalProvidersConfig::default(),
        ));
        let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
        let key = SigningKey::from_slice(&[1; 32]).unwrap();
        service.ecdsa_verifying_key = Arc::new(*key.verifying_key());
        service.ecdsa_signing_key = Arc::new(key);

        let (signature, address) = service
            .sign_ecdsa_gateway_signature("request-hash:response-hash")
            .unwrap();

        // Recorded using k256 0.13.4 before the dependency upgrade. This checks
        // EIP-191 framing, deterministic signing, low-S encoding and Ethereum v.
        assert_eq!(
            signature,
            concat!(
                "0x",
                "c4949d98d11acafc9db9059591c185ecc1ba39048a8b6b95524d9317bee50f4538",
                "37a274a9db76a3075e2ad3c8f37908c05fe35791e39cdc66d55bb6720ecb031c",
            ),
        );
        assert_eq!(address, "0x1a642f0e3c3af545e7acbd38b07251b3990914f1");

        let bytes = hex::decode(signature.trim_start_matches("0x")).unwrap();
        let signature = Signature::from_slice(&bytes[..64]).unwrap();
        let recovery_id = RecoveryId::from_byte(bytes[64] - 27).unwrap();
        let digest =
            Keccak256::digest(b"\x19Ethereum Signed Message:\n26request-hash:response-hash");
        let recovered =
            VerifyingKey::recover_from_prehash(&digest, &signature, recovery_id).unwrap();

        assert_eq!(recovered, *service.ecdsa_verifying_key);
    }
}
