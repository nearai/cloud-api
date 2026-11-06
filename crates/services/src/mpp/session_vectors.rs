//! Test-only access to the golden vectors for the `session` intent
//! (`testdata/session_vectors.json`, produced with ox/viem/mppx; see the README there).

use serde_json::Value;

use super::abi::ChannelDescriptor;

const JSON: &str = include_str!("testdata/session_vectors.json");

pub(crate) struct Vectors(Value);

impl Vectors {
    pub(crate) fn load() -> Self {
        Self(serde_json::from_str(JSON).unwrap())
    }

    fn at(&self, path: &str) -> &Value {
        path.split('.')
            .fold(&self.0, |value, key| value.get(key).unwrap())
    }

    pub(crate) fn text(&self, path: &str) -> &str {
        self.at(path).as_str().unwrap()
    }

    pub(crate) fn bytes(&self, path: &str) -> Vec<u8> {
        hex::decode(self.text(path).strip_prefix("0x").unwrap()).unwrap()
    }

    pub(crate) fn fixed<const N: usize>(&self, path: &str) -> [u8; N] {
        self.bytes(path).try_into().unwrap()
    }

    /// An integer given as a JSON string or number.
    pub(crate) fn int(&self, path: &str) -> u128 {
        match self.at(path) {
            Value::String(text) => text.parse().unwrap(),
            Value::Number(number) => u128::from(number.as_u64().unwrap()),
            other => panic!("not an integer: {other}"),
        }
    }

    pub(crate) fn chain_id(&self) -> u64 {
        u64::try_from(self.int("chainId")).unwrap()
    }

    pub(crate) fn escrow(&self) -> [u8; 20] {
        self.fixed("escrow")
    }

    pub(crate) fn descriptor(&self) -> ChannelDescriptor {
        ChannelDescriptor {
            payer: self.fixed("descriptor.payer"),
            payee: self.fixed("descriptor.payee"),
            operator: self.fixed("descriptor.operator"),
            token: self.fixed("descriptor.token"),
            salt: self.fixed("descriptor.salt"),
            authorized_signer: self.fixed("descriptor.authorizedSigner"),
            expiring_nonce_hash: self.fixed("descriptor.expiringNonceHash"),
        }
    }

    pub(crate) fn secret_key(&self, path: &str) -> k256::SecretKey {
        k256::SecretKey::from_slice(&self.bytes(path)).unwrap()
    }
}
