//! The payee's signer: a secp256k1 key held in memory, from `MPP_TEMPO_SIGNER_KEY`.
//!
//! The key never appears in `Debug` output, logs or errors; only its address does.

use k256::SecretKey;

use super::ports::TempoSigner;
use super::secp256k1::{address_of, sign_digest, SigError, SIGNATURE_LEN};

/// A payee signer holding its key in process memory.
pub struct LocalSigner {
    key: SecretKey,
    address: [u8; 20],
}

impl std::fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalSigner { key: <redacted> }")
    }
}

/// Why a signer key was refused. Carries nothing of the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignerKeyError {
    /// Not 32 bytes of hex (with or without `0x`).
    #[error("signer key is not 32 bytes of hex")]
    Format,
    /// Zero or not below the curve order.
    #[error("signer key is not a valid secp256k1 scalar")]
    Scalar,
}

impl LocalSigner {
    pub fn new(key: SecretKey) -> Self {
        let address = address_of(&key);
        Self { key, address }
    }

    /// Parse a 32-byte hex key (`0x` optional).
    pub fn from_hex(raw: &str) -> Result<Self, SignerKeyError> {
        let digits = raw.trim();
        let digits = digits.strip_prefix("0x").unwrap_or(digits);
        if digits.len() != 64 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SignerKeyError::Format);
        }
        let bytes = hex::decode(digits).map_err(|_| SignerKeyError::Format)?;
        let key = SecretKey::from_slice(&bytes).map_err(|_| SignerKeyError::Scalar)?;
        Ok(Self::new(key))
    }
}

impl TempoSigner for LocalSigner {
    fn address(&self) -> [u8; 20] {
        self.address
    }

    fn sign(&self, digest: &[u8; 32]) -> Result<[u8; SIGNATURE_LEN], SigError> {
        sign_digest(&self.key, digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::secp256k1::recover_address;
    use crate::mpp::session_vectors::Vectors;

    #[test]
    fn the_vector_payee_key_signs_as_the_vector_payee() {
        let v = Vectors::load();
        let signer = LocalSigner::from_hex(v.text("payeeKey")).unwrap();
        assert_eq!(signer.address(), v.fixed::<20>("payee"));
        let digest = [0x5a; 32];
        let signature = signer.sign(&digest).unwrap();
        assert_eq!(recover_address(&digest, &signature), Ok(signer.address()));
        assert_eq!(format!("{signer:?}"), "LocalSigner { key: <redacted> }");
    }

    #[test]
    fn malformed_and_invalid_keys_are_refused_without_echo() {
        for raw in [
            "",
            "0x",
            "0x12",
            "zz".repeat(32).as_str(),
            "00".repeat(33).as_str(),
        ] {
            assert_eq!(
                LocalSigner::from_hex(raw).err(),
                Some(SignerKeyError::Format),
                "{raw:?}"
            );
        }
        assert_eq!(
            LocalSigner::from_hex(&"00".repeat(32)).err(),
            Some(SignerKeyError::Scalar)
        );
        // Group order: not below n.
        assert_eq!(
            LocalSigner::from_hex(
                "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
            )
            .err(),
            Some(SignerKeyError::Scalar)
        );
        assert!(LocalSigner::from_hex(&"01".repeat(32)).is_ok());
    }
}
