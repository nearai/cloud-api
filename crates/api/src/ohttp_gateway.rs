use ohttp::hpke::{Aead, Kdf, Kem};
use ohttp::{KeyConfig, Server, ServerResponse, SymmetricSuite};

/// Arbitrary key ID for this deployment's OHTTP key configuration.
const OHTTP_KEY_ID: u8 = 1;

/// OHTTP Gateway (RFC 9458) — handles HPKE decapsulation/encapsulation.
///
/// The HPKE keypair is deterministically derived from the Ed25519 signing key
/// seed (loaded from dstack KMS). All cloud-api instances share the same KMS
/// key, so they produce identical OHTTP key configurations — clients can
/// encrypt to any instance.
pub struct OhttpGateway {
    server: Server,
    /// Pre-encoded key configuration bytes for the well-known endpoint.
    config_bytes: Vec<u8>,
}

impl OhttpGateway {
    /// Create from Ed25519 secret key material (32 bytes from dstack KMS).
    ///
    /// Uses HPKE `DeriveKeyPair` internally — the resulting X25519 keypair is
    /// domain-separated from the E2EE X25519 key (different derivation path).
    pub fn new(ed25519_secret: &[u8; 32]) -> anyhow::Result<Self> {
        let config = KeyConfig::derive(
            OHTTP_KEY_ID,
            Kem::X25519Sha256,
            vec![
                SymmetricSuite::new(Kdf::HkdfSha256, Aead::Aes128Gcm),
                SymmetricSuite::new(Kdf::HkdfSha256, Aead::ChaCha20Poly1305),
            ],
            ed25519_secret,
        )?;
        let config_bytes = config.encode()?;
        let server = Server::new(config)?;
        Ok(Self {
            server,
            config_bytes,
        })
    }

    /// Encoded key configuration bytes (served at `/.well-known/ohttp-gateway`).
    pub fn config_bytes(&self) -> &[u8] {
        &self.config_bytes
    }

    /// Decapsulate a standard OHTTP request (RFC 9458).
    pub fn decapsulate(
        &self,
        enc_request: &[u8],
    ) -> Result<(Vec<u8>, ServerResponse), ohttp::Error> {
        self.server.decapsulate(enc_request)
    }

    /// Clone the inner `Server` for use with streaming APIs.
    pub fn clone_server(&self) -> Server {
        self.server.clone()
    }
}

/// OHTTP attestation payload included in `GET /v1/attestation/report`.
///
/// Clients verify `signature` (Ed25519 over decoded `key_config` bytes) against
/// the attested `signing_key` to confirm the OHTTP public key is bound to the TEE.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct OhttpAttestation {
    pub signing_algo: String,
    pub signing_key: String,
    /// Hex-encoded OHTTP key configuration bytes (RFC 9458).
    pub key_config: String,
    /// Ed25519 signature over the decoded `key_config` bytes.
    pub signature: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
        0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
        0x1c, 0xae, 0x7f, 0x60,
    ];

    #[test]
    fn test_deterministic_config() {
        let gw1 = OhttpGateway::new(&TEST_KEY).unwrap();
        let gw2 = OhttpGateway::new(&TEST_KEY).unwrap();
        assert_eq!(gw1.config_bytes(), gw2.config_bytes());
    }

    #[test]
    fn test_roundtrip() {
        let gw = OhttpGateway::new(&TEST_KEY).unwrap();

        let mut config = KeyConfig::decode(gw.config_bytes()).unwrap();
        let client_request = ohttp::ClientRequest::from_config(&mut config).unwrap();

        let inner = b"hello from client";
        let (enc_request, client_response) = client_request.encapsulate(inner).unwrap();

        let (plaintext, server_response) = gw.decapsulate(&enc_request).unwrap();
        assert_eq!(plaintext, inner);

        let inner_resp = b"hello from server";
        let enc_response = server_response.encapsulate(inner_resp).unwrap();

        let decrypted = client_response.decapsulate(&enc_response).unwrap();
        assert_eq!(decrypted, inner_resp);
    }

    #[test]
    fn test_different_keys_produce_different_configs() {
        let gw_a = OhttpGateway::new(&[0x01u8; 32]).unwrap();
        let gw_b = OhttpGateway::new(&[0x02u8; 32]).unwrap();
        assert_ne!(gw_a.config_bytes(), gw_b.config_bytes());
    }
}
