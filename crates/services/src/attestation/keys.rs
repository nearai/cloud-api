use dstack_sdk::dstack_client;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use k256::ecdsa::{SigningKey as EcdsaSigningKey, VerifyingKey as EcdsaVerifyingKey};
use rand_core::OsRng;
use sha3::{Digest, Keccak256};

use super::{AttestationError, AttestationService};

const GATEWAY_KEY_PATH_ED25519: &str = "/signing-key/ed25519";
const GATEWAY_KEY_PATH_ECDSA: &str = "/signing-key/ecdsa";

impl AttestationService {
    #[cfg(debug_assertions)]
    pub(in crate::attestation) fn generate_ephemeral_signing_keys(
    ) -> (SigningKey, VerifyingKey, EcdsaSigningKey, EcdsaVerifyingKey) {
        let mut csprng = OsRng;
        let ed25519_signing_key = SigningKey::generate(&mut csprng);
        let ed25519_verifying_key = ed25519_signing_key.verifying_key();
        let ed25519_address = hex::encode(ed25519_verifying_key.as_bytes());
        tracing::info!(
            "Generated ed25519 key pair for response signing (ephemeral). Public key (signing address): {}",
            ed25519_address
        );

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

    pub(in crate::attestation) async fn derive_signing_keys_from_dstack(
    ) -> Result<(SigningKey, VerifyingKey, EcdsaSigningKey, EcdsaVerifyingKey), AttestationError>
    {
        let client = dstack_client::DstackClient::new(None);
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

    fn ecdsa_public_key_to_ethereum_address(verifying_key: &EcdsaVerifyingKey) -> Vec<u8> {
        let encoded_point = verifying_key.to_encoded_point(false);
        let point_bytes = encoded_point.as_bytes();
        let uncompressed_pubkey = &point_bytes[1..65];
        let hash = Keccak256::digest(uncompressed_pubkey);
        hash[12..32].to_vec()
    }

    pub fn ed25519_secret_bytes(&self) -> [u8; 32] {
        self.ed25519_signing_key.to_bytes()
    }

    pub fn sign_ohttp_attestation(&self, data: &[u8]) -> (String, String) {
        let sig = self.ed25519_signing_key.sign(data);
        let signature = hex::encode(sig.to_bytes());
        let signing_key_hex = hex::encode(self.ed25519_verifying_key.as_bytes());
        (signature, signing_key_hex)
    }

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
}
