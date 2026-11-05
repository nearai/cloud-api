//! Fakes and fixtures for tests of the MPP service and its middleware. Compiled for this
//! crate's tests and, through the `test-mocks` feature, for the API crate's tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::memory::InMemoryClaimRepository;
use super::ports::{
    ClaimLease, ClaimOutcome, PaymentClaimRepository, RpcError, RpcLog, RpcReceipt, ServeOutcome,
    TempoRpc,
};
use super::protocol::{b64url_encode, Challenge};
use super::tempo::{attribution_memo, transaction_hash, TRANSFER_TOPIC, TRANSFER_WITH_MEMO_TOPIC};
use super::transaction::TRANSFER_WITH_MEMO_SELECTOR;
use crate::common::RepositoryError;

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
    /// Never answers (a node that accepts the connection and goes silent).
    Hang,
}

/// A scripted Tempo node: a table of mined receipts and a failure mode.
pub struct FakeTempo {
    mined: Mutex<HashMap<[u8; 32], RpcReceipt>>,
    send_mode: Mutex<SendMode>,
    lookup_error: Mutex<Option<RpcError>>,
    /// Returned by every successful broadcast instead of the transaction's own receipt.
    send_receipt: Mutex<Option<RpcReceipt>>,
    /// Block timestamps (Unix seconds) by number; unknown blocks are `None`.
    block_times: Mutex<HashMap<u64, u64>>,
    /// A node that never answers block lookups.
    block_lookup_hangs: std::sync::atomic::AtomicBool,
    sends: AtomicUsize,
    lookups: AtomicUsize,
    /// Answers `eth_call` from the target and calldata; `None` answers every call with
    /// [`RpcError::Rejected`].
    call_handler: Mutex<Option<CallHandler>>,
    gas_price: Mutex<Result<u128, RpcError>>,
    calls: AtomicUsize,
}

/// A scripted `eth_call`: `(to, calldata) -> return data`.
pub type CallHandler = Box<dyn Fn([u8; 20], &[u8]) -> Result<Vec<u8>, RpcError> + Send + Sync>;

impl Default for FakeTempo {
    fn default() -> Self {
        Self {
            mined: Mutex::new(HashMap::new()),
            send_mode: Mutex::new(SendMode::Normal),
            lookup_error: Mutex::new(None),
            send_receipt: Mutex::new(None),
            block_times: Mutex::new(HashMap::new()),
            block_lookup_hangs: std::sync::atomic::AtomicBool::new(false),
            sends: AtomicUsize::new(0),
            lookups: AtomicUsize::new(0),
            call_handler: Mutex::new(None),
            gas_price: Mutex::new(Ok(600_000_001)),
            calls: AtomicUsize::new(0),
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

    /// Make every broadcast answer with `receipt`, whatever transaction was sent (a node that
    /// returns the wrong receipt).
    pub fn set_send_receipt(&self, receipt: Option<RpcReceipt>) {
        if let Ok(mut current) = self.send_receipt.lock() {
            *current = receipt;
        }
    }

    /// Record when block `number` was produced.
    pub fn set_block_time(&self, number: u64, timestamp: u64) {
        if let Ok(mut times) = self.block_times.lock() {
            times.insert(number, timestamp);
        }
    }

    pub fn set_block_lookup_hangs(&self, hangs: bool) {
        self.block_lookup_hangs.store(hangs, Ordering::SeqCst);
    }

    /// Script `eth_call`.
    pub fn set_call_handler(&self, handler: CallHandler) {
        if let Ok(mut current) = self.call_handler.lock() {
            *current = Some(handler);
        }
    }

    /// Script `eth_gasPrice`.
    pub fn set_gas_price(&self, price: Result<u128, RpcError>) {
        if let Ok(mut current) = self.gas_price.lock() {
            *current = price;
        }
    }

    /// How many `eth_call`s were made.
    pub fn eth_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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
            SendMode::Hang => std::future::pending().await,
            SendMode::Normal => {
                if let Some(receipt) = self.send_receipt.lock().ok().and_then(|r| r.clone()) {
                    return Ok(receipt);
                }
                self.mined
                    .lock()
                    .ok()
                    .and_then(|mined| mined.get(&transaction_hash(raw)).cloned())
                    .ok_or(RpcError::Rejected)
            }
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

    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
        if self.block_lookup_hangs.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        if let Some(error) = self.lookup_error.lock().ok().and_then(|e| *e) {
            return Err(error);
        }
        Ok(self
            .block_times
            .lock()
            .ok()
            .and_then(|times| times.get(&number).copied()))
    }

    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.lookup_error.lock().ok().and_then(|e| *e) {
            return Err(error);
        }
        let handler = self
            .call_handler
            .lock()
            .map_err(|_| RpcError::Unavailable)?;
        match handler.as_ref() {
            Some(handler) => handler(to, data),
            None => Err(RpcError::Rejected),
        }
    }

    async fn gas_price(&self) -> Result<u128, RpcError> {
        *self.gas_price.lock().map_err(|_| RpcError::Unavailable)?
    }
}

/// A claim store that can be told to fail, over a real in-memory one.
#[derive(Default)]
pub struct FlakyClaimRepository {
    inner: std::sync::Arc<InMemoryClaimRepository>,
    fail_claims: AtomicBool,
    fail_finishes: AtomicBool,
    fail_purges: AtomicBool,
}

impl FlakyClaimRepository {
    /// The store underneath, for assertions.
    pub fn store(&self) -> &InMemoryClaimRepository {
        &self.inner
    }

    pub fn set_fail_claims(&self, fail: bool) {
        self.fail_claims.store(fail, Ordering::SeqCst);
    }

    pub fn set_fail_finishes(&self, fail: bool) {
        self.fail_finishes.store(fail, Ordering::SeqCst);
    }

    pub fn set_fail_purges(&self, fail: bool) {
        self.fail_purges.store(fail, Ordering::SeqCst);
    }
}

fn store_down() -> RepositoryError {
    RepositoryError::ConnectionFailed("claim store down".to_string())
}

#[async_trait]
impl PaymentClaimRepository for FlakyClaimRepository {
    async fn claim(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<ClaimOutcome, RepositoryError> {
        if self.fail_claims.load(Ordering::SeqCst) {
            return Err(store_down());
        }
        self.inner.claim(payment_key, challenge_id).await
    }

    async fn is_spent(
        &self,
        payment_key: &str,
        challenge_id: &str,
    ) -> Result<bool, RepositoryError> {
        if self.fail_claims.load(Ordering::SeqCst) {
            return Err(store_down());
        }
        self.inner.is_spent(payment_key, challenge_id).await
    }

    async fn finish(
        &self,
        payment_key: &str,
        lease: ClaimLease,
        outcome: ServeOutcome,
    ) -> Result<(), RepositoryError> {
        if self.fail_finishes.load(Ordering::SeqCst) {
            return Err(store_down());
        }
        self.inner.finish(payment_key, lease, outcome).await
    }

    async fn purge_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, RepositoryError> {
        if self.fail_purges.load(Ordering::SeqCst) {
            return Err(store_down());
        }
        self.inner.purge_older_than(cutoff).await
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
        block_number: 1,
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

/// A framing-valid (`0x76` + one RLP list) `transaction` payload that is not a payment: the
/// bytes depend on `seed`. The service refuses it before any broadcast.
pub fn framed_transaction_payload(seed: u8) -> (Value, Vec<u8>) {
    let mut raw = vec![0x76, 0xc0 + 20];
    raw.extend([seed; 20]);
    (transaction_payload(&raw), raw)
}

/// A `transaction` payload carrying `raw`.
pub fn transaction_payload(raw: &[u8]) -> Value {
    serde_json::json!({"type": "transaction", "signature": format!("0x{}", hex::encode(raw))})
}

// ----- RLP encoding and Tempo transactions, for building what a client sends -----

/// RLP of a byte string.
pub fn rlp_string(content: &[u8]) -> Vec<u8> {
    match content {
        [byte] if *byte < 0x80 => vec![*byte],
        content if content.len() <= 55 => {
            let mut out = vec![0x80 + content.len() as u8];
            out.extend(content);
            out
        }
        content => {
            let mut out = rlp_long_header(0xb7, content.len());
            out.extend(content);
            out
        }
    }
}

/// RLP of a list of already-encoded items.
pub fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = if payload.len() <= 55 {
        vec![0xc0 + payload.len() as u8]
    } else {
        rlp_long_header(0xf7, payload.len())
    };
    out.extend(payload);
    out
}

/// RLP of an unsigned integer (zero is the empty string).
pub fn rlp_uint(value: u128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    rlp_string(bytes.get(first..).unwrap_or_default())
}

fn rlp_long_header(base: u8, len: usize) -> Vec<u8> {
    let length: Vec<u8> = len
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let mut out = vec![base + length.len() as u8];
    out.extend(length);
    out
}

/// `transferWithMemo(address,uint256,bytes32)` calldata.
pub fn transfer_with_memo_input(recipient: [u8; 20], amount: u128, memo: [u8; 32]) -> Vec<u8> {
    let mut input = TRANSFER_WITH_MEMO_SELECTOR.to_vec();
    input.extend([0u8; 12]);
    input.extend(recipient);
    input.extend(amount_word(amount));
    input.extend(memo);
    input
}

/// One call of a Tempo transaction: `(to, value, input)`.
pub type TxCall = ([u8; 20], u128, Vec<u8>);

/// A signed Tempo transaction (`0x76 || rlp([...])`) shaped as `mppx` builds one for a charge:
/// an expiring nonce key, a fee token, no fee payer, no authorization list, and a 65-byte
/// secp256k1 signature last. `seed` varies the bytes (the expiry), hence the hash.
pub fn tempo_transaction(chain_id: u64, calls: &[TxCall], seed: u64) -> Vec<u8> {
    let calls: Vec<Vec<u8>> = calls
        .iter()
        .map(|(to, value, input)| rlp_list(&[rlp_string(to), rlp_uint(*value), rlp_string(input)]))
        .collect();
    let fields = [
        rlp_uint(u128::from(chain_id)),
        rlp_uint(1_000_000_000),  // max_priority_fee_per_gas
        rlp_uint(20_000_000_000), // max_fee_per_gas
        rlp_uint(150_000),        // gas_limit
        rlp_list(&calls),
        rlp_list(&[]),                              // access_list
        rlp_string(&[0xff; 32]),                    // nonce_key: the expiring nonce key (U256::MAX)
        rlp_uint(0),                                // nonce
        rlp_uint(1_900_000_000 + u128::from(seed)), // valid_before
        rlp_uint(0),                                // valid_after
        rlp_string(&[0x20; 20]),                    // fee_token
        rlp_uint(0),                                // fee_payer_signature: none
        rlp_list(&[]),                              // tempo_authorization_list
        rlp_string(&[0x5a; 65]),                    // signature
    ];
    let mut raw = vec![0x76];
    raw.extend(rlp_list(&fields));
    raw
}

/// What a charge transaction must pay.
#[derive(Debug, Clone, Copy)]
pub struct PaymentTx<'a> {
    pub chain_id: u64,
    pub currency: [u8; 20],
    pub recipient: [u8; 20],
    pub amount: u128,
    pub realm: &'a str,
    pub challenge_id: &'a str,
    pub seed: u64,
}

/// A transaction paying `tx`: one `transferWithMemo` of `amount` to `recipient` in
/// `currency`, with the attribution memo for the challenge.
pub fn payment_transaction(tx: &PaymentTx<'_>) -> Vec<u8> {
    let memo = attribution_memo(tx.realm, tx.challenge_id);
    tempo_transaction(
        tx.chain_id,
        &[(
            tx.currency,
            0,
            transfer_with_memo_input(tx.recipient, tx.amount, memo),
        )],
        tx.seed,
    )
}
