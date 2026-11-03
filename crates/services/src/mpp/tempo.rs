//! Verification of a Tempo charge against a transaction receipt.
//!
//! Mirrors the reference (`mpp` 0.18.1 `tempo/attribution.rs` and
//! `protocol/methods/tempo/method/{receipt_logs,matching,memo,hash_credential}.rs`):
//!
//! - TIP-20 `Transfer` and `TransferWithMemo` logs are read; a `transferWithMemo` emits
//!   both for one transfer, and an adjacent pair is merged so one transfer counts once.
//! - The payment is the transfer from the configured currency contract to the configured
//!   recipient for exactly the requested amount, sent by the expected sender: the `source`
//!   DID address when the client sent one, else the transaction sender.
//! - The matched transfer must carry an MPP attribution memo bound to this realm and this
//!   challenge id. An MPP memo for another challenge or realm on the matched transfer is
//!   refused. Memos that are not MPP memos are ignored.

use sha3::{Digest, Keccak256};

use super::ports::RpcReceipt;

/// `keccak256("Transfer(address,address,uint256)")`.
pub const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
/// `keccak256("TransferWithMemo(address,address,uint256,bytes32)")`.
pub const TRANSFER_WITH_MEMO_TOPIC: [u8; 32] = [
    0x57, 0xbc, 0x73, 0x54, 0xaa, 0x85, 0xae, 0xd3, 0x39, 0xe0, 0x00, 0xbc, 0xcf, 0xfa, 0xbb, 0xc5,
    0x29, 0x46, 0x6a, 0xf3, 0x5f, 0x07, 0x72, 0xc8, 0xf8, 0xee, 0x11, 0x45, 0x92, 0x7d, 0xe7, 0xf0,
];
/// `keccak256("mpp")[0..4]`, the on-chain tag of an MPP attribution memo.
pub const MPP_MEMO_TAG: [u8; 4] = [0xef, 0x1e, 0xd7, 0x12];
/// Attribution memo version byte.
pub const MPP_MEMO_VERSION: u8 = 0x01;

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// The transaction hash of a signed Tempo transaction: `keccak256` of its encoded bytes.
pub fn transaction_hash(raw: &[u8]) -> [u8; 32] {
    keccak256(raw)
}

fn first_bytes<const N: usize>(hash: [u8; 32]) -> [u8; N] {
    let mut out = [0u8; N];
    for (slot, byte) in out.iter_mut().zip(hash) {
        *slot = byte;
    }
    out
}

/// The 32-byte attribution memo a client puts on its transfer for `challenge_id` at `realm`
/// (no client fingerprint):
/// `tag(4) | version(1) | keccak(realm)[0..10] | zeros(10) | keccak(challenge_id)[0..7]`.
pub fn attribution_memo(realm: &str, challenge_id: &str) -> [u8; 32] {
    let mut memo = [0u8; 32];
    memo[..4].copy_from_slice(&MPP_MEMO_TAG);
    memo[4] = MPP_MEMO_VERSION;
    memo[5..15].copy_from_slice(&first_bytes::<10>(keccak256(realm.as_bytes())));
    // memo[15..25] is the client fingerprint: zeros when anonymous.
    memo[25..32].copy_from_slice(&first_bytes::<7>(keccak256(challenge_id.as_bytes())));
    memo
}

/// Whether a memo is an MPP attribution memo (tag and version).
pub fn is_mpp_memo(memo: &[u8; 32]) -> bool {
    memo[..4] == MPP_MEMO_TAG && memo[4] == MPP_MEMO_VERSION
}

/// Whether an MPP memo is bound to `realm` and `challenge_id`.
pub fn is_bound_memo(memo: &[u8; 32], realm: &str, challenge_id: &str) -> bool {
    let expected = attribution_memo(realm, challenge_id);
    is_mpp_memo(memo) && memo[5..15] == expected[5..15] && memo[25..32] == expected[25..32]
}

/// One TIP-20 transfer read from a receipt log. The amount stays a 32-byte big-endian word,
/// the on-chain `uint256`: a value above `u128` simply never equals a requested amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransferEffect {
    token: [u8; 20],
    from: [u8; 20],
    to: [u8; 20],
    amount: [u8; 32],
    memo: Option<[u8; 32]>,
}

fn topic_address(topic: &[u8; 32]) -> Option<[u8; 20]> {
    let (padding, address) = topic.split_at(12);
    if padding.iter().any(|b| *b != 0) {
        return None;
    }
    address.try_into().ok()
}

fn parse_log(log: &super::ports::RpcLog) -> Option<TransferEffect> {
    let topic0 = log.topics.first()?;
    let amount: [u8; 32] = log.data.as_slice().try_into().ok()?;
    let memo = if *topic0 == TRANSFER_TOPIC && log.topics.len() == 3 {
        None
    } else if *topic0 == TRANSFER_WITH_MEMO_TOPIC && log.topics.len() == 4 {
        Some(*log.topics.get(3)?)
    } else {
        return None;
    };
    Some(TransferEffect {
        token: log.address,
        from: topic_address(log.topics.get(1)?)?,
        to: topic_address(log.topics.get(2)?)?,
        amount,
        memo,
    })
}

/// A `Transfer` and the `TransferWithMemo` that follows it for the same transfer.
fn is_twin(a: &TransferEffect, b: &TransferEffect) -> bool {
    a.memo.is_some() != b.memo.is_some()
        && a.token == b.token
        && a.from == b.from
        && a.to == b.to
        && a.amount == b.amount
}

/// One effect per token transfer. As in the reference, only the immediately following log
/// can be a twin, and an unparseable log is not one.
fn transfer_effects(receipt: &RpcReceipt) -> Vec<TransferEffect> {
    let parsed: Vec<Option<TransferEffect>> = receipt.logs.iter().map(parse_log).collect();
    let mut effects = Vec::new();
    let mut index = 0;
    while let Some(current) = parsed.get(index) {
        index += 1;
        let Some(current) = current else {
            continue;
        };
        let twin = parsed
            .get(index)
            .and_then(Option::as_ref)
            .filter(|next| is_twin(current, next));
        if let Some(twin) = twin {
            index += 1;
            effects.push(if current.memo.is_some() {
                *current
            } else {
                *twin
            });
        } else {
            effects.push(*current);
        }
    }
    effects
}

/// What the server asked for and what it binds the payment to.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub currency: [u8; 20],
    pub recipient: [u8; 20],
    pub amount: u128,
    /// The `source` DID address, when the client sent one.
    pub source: Option<[u8; 20]>,
    pub realm: &'a str,
    pub challenge_id: &'a str,
}

/// Why a receipt does not pay a challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptVerdict {
    /// The transaction reverted.
    Reverted,
    /// No transfer from the configured sender matches currency, recipient and amount.
    NoMatchingTransfer,
    /// A matching transfer exists but its memo is not bound to this challenge. The funds
    /// have already moved, so another payment would not help: the caller answers as a
    /// consumed credential, not as a retryable failure.
    MemoNotBound,
}

/// Verify a receipt pays `expected`. `Ok` means a matching transfer carries an MPP memo
/// bound to the realm and challenge id.
pub fn verify_receipt(receipt: &RpcReceipt, expected: &Expected<'_>) -> Result<(), ReceiptVerdict> {
    if !receipt.success {
        return Err(ReceiptVerdict::Reverted);
    }
    let sender = expected.source.unwrap_or(receipt.from);
    let mut amount = [0u8; 32];
    amount[16..].copy_from_slice(&expected.amount.to_be_bytes());

    let effects = transfer_effects(receipt);
    let matches = |effect: &&TransferEffect| {
        effect.token == expected.currency
            && effect.to == expected.recipient
            && effect.amount == amount
            && effect.from == sender
    };
    // A memo transfer is preferred over a plain one, as in the reference.
    let matched = effects
        .iter()
        .filter(matches)
        .find(|effect| effect.memo.is_some())
        .or_else(|| {
            effects
                .iter()
                .filter(matches)
                .find(|effect| effect.memo.is_none())
        })
        .ok_or(ReceiptVerdict::NoMatchingTransfer)?;

    match matched.memo {
        Some(memo) if is_bound_memo(&memo, expected.realm, expected.challenge_id) => Ok(()),
        _ => Err(ReceiptVerdict::MemoNotBound),
    }
}

#[cfg(test)]
#[path = "tempo_tests.rs"]
mod tests;
