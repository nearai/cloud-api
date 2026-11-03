//! Fakes and fixtures for tests of the MPP service and its middleware. Compiled for this
//! crate's tests and, through the `test-mocks` feature, for the API crate's tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::ports::{RpcError, RpcLog, RpcReceipt, TempoRpc};
use super::protocol::{b64url_encode, Challenge};
use super::tempo::{attribution_memo, transaction_hash, TRANSFER_TOPIC, TRANSFER_WITH_MEMO_TOPIC};

/// How `eth_sendRawTransactionSync` behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// Returns the receipt of the transaction if it is mined, else a JSON-RPC error.
    Normal,
    /// Always a JSON-RPC error (as for a transaction that is already known); the receipt can
    /// still be looked up.
    Reject,
    /// Transport failure.
    Unavailable,
}

/// A scripted Tempo node: a table of mined receipts and a failure mode.
pub struct FakeTempo {
    mined: Mutex<HashMap<[u8; 32], RpcReceipt>>,
    send_mode: Mutex<SendMode>,
    lookup_error: Mutex<Option<RpcError>>,
    sends: AtomicUsize,
    lookups: AtomicUsize,
}

impl Default for FakeTempo {
    fn default() -> Self {
        Self {
            mined: Mutex::new(HashMap::new()),
            send_mode: Mutex::new(SendMode::Normal),
            lookup_error: Mutex::new(None),
            sends: AtomicUsize::new(0),
            lookups: AtomicUsize::new(0),
        }
    }
}

impl FakeTempo {
    /// Mine a receipt under its own hash.
    pub fn mine(&self, receipt: RpcReceipt) {
        if let Ok(mut mined) = self.mined.lock() {
            mined.insert(receipt.tx_hash, receipt);
        }
    }

    /// Serve `receipt` for lookups of `hash` (a node that answers with the wrong receipt).
    pub fn mined_alias(&self, hash: [u8; 32], receipt: RpcReceipt) {
        if let Ok(mut mined) = self.mined.lock() {
            mined.insert(hash, receipt);
        }
    }

    pub fn set_send_mode(&self, mode: SendMode) {
        if let Ok(mut current) = self.send_mode.lock() {
            *current = mode;
        }
    }

    pub fn set_lookup_error(&self, error: Option<RpcError>) {
        if let Ok(mut current) = self.lookup_error.lock() {
            *current = error;
        }
    }

    pub fn send_calls(&self) -> usize {
        self.sends.load(Ordering::SeqCst)
    }

    pub fn lookup_calls(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TempoRpc for FakeTempo {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        let mode = self
            .send_mode
            .lock()
            .map(|m| *m)
            .unwrap_or(SendMode::Unavailable);
        match mode {
            SendMode::Unavailable => Err(RpcError::Unavailable),
            SendMode::Reject => Err(RpcError::Rejected),
            SendMode::Normal => self
                .mined
                .lock()
                .ok()
                .and_then(|mined| mined.get(&transaction_hash(raw)).cloned())
                .ok_or(RpcError::Rejected),
        }
    }

    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.lookup_error.lock().ok().and_then(|e| *e) {
            return Err(error);
        }
        Ok(self
            .mined
            .lock()
            .ok()
            .and_then(|mined| mined.get(&hash).cloned()))
    }
}

fn address_topic(address: [u8; 20]) -> [u8; 32] {
    let mut topic = [0u8; 32];
    topic[12..].copy_from_slice(&address);
    topic
}

fn amount_word(amount: u128) -> Vec<u8> {
    let mut data = vec![0u8; 32];
    data[16..].copy_from_slice(&amount.to_be_bytes());
    data
}

/// The receipt of a successful `transferWithMemo` of `amount` from `from` to `recipient` in
/// `currency`: `Transfer` then `TransferWithMemo`, as Tempo emits them. `memo` of `None`
/// emits a plain `Transfer` only.
pub fn payment_receipt(
    tx_hash: [u8; 32],
    from: [u8; 20],
    currency: [u8; 20],
    recipient: [u8; 20],
    amount: u128,
    memo: Option<[u8; 32]>,
) -> RpcReceipt {
    let mut logs = vec![RpcLog {
        address: currency,
        topics: vec![
            TRANSFER_TOPIC,
            address_topic(from),
            address_topic(recipient),
        ],
        data: amount_word(amount),
    }];
    if let Some(memo) = memo {
        logs.push(RpcLog {
            address: currency,
            topics: vec![
                TRANSFER_WITH_MEMO_TOPIC,
                address_topic(from),
                address_topic(recipient),
                memo,
            ],
            data: amount_word(amount),
        });
    }
    RpcReceipt {
        tx_hash,
        success: true,
        from,
        logs,
    }
}

/// `payment_receipt` with the attribution memo a compliant client attaches for `challenge`.
pub fn bound_payment_receipt(
    tx_hash: [u8; 32],
    from: [u8; 20],
    currency: [u8; 20],
    recipient: [u8; 20],
    amount: u128,
    realm: &str,
    challenge: &Challenge,
) -> RpcReceipt {
    payment_receipt(
        tx_hash,
        from,
        currency,
        recipient,
        amount,
        Some(attribution_memo(realm, &challenge.id)),
    )
}

/// `Authorization` value a client sends back for `challenge`.
pub fn credential_header(challenge: &Challenge, payload: Value, source: Option<&str>) -> String {
    let mut credential = serde_json::json!({
        "challenge": {
            "id": challenge.id,
            "realm": challenge.realm,
            "method": "tempo",
            "intent": "charge",
            "request": challenge.request,
            "expires": challenge.expires_string(),
            "digest": challenge.digest,
            "opaque": challenge.opaque,
        },
        "payload": payload,
    });
    if let Some(source) = source {
        credential["source"] = Value::String(source.to_string());
    }
    format!(
        "Payment {}",
        b64url_encode(credential.to_string().as_bytes())
    )
}

/// A `hash` payload for `tx_hash`.
pub fn hash_payload(tx_hash: [u8; 32]) -> Value {
    serde_json::json!({"type": "hash", "hash": format!("0x{}", hex::encode(tx_hash))})
}

/// A well-formed (`0x76` + one RLP list) `transaction` payload whose bytes depend on `seed`.
pub fn transaction_payload(seed: u8) -> (Value, Vec<u8>) {
    let mut raw = vec![0x76, 0xc0 + 20];
    raw.extend([seed; 20]);
    (
        serde_json::json!({"type": "transaction", "signature": format!("0x{}", hex::encode(&raw))}),
        raw,
    )
}
