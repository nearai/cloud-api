//! A typed Tempo transaction (type `0x76`) in the one shape the `session` intent uses:
//! a secp256k1-signed transaction with calls, an optional validity window and fee token,
//! and nothing else.
//!
//! Wire layout (`tempo-primitives` 1.11 `TempoTransaction`):
//!
//! ```text
//! 0x76 || rlp([chain_id, max_priority_fee_per_gas, max_fee_per_gas, gas_limit,
//!              calls = [[to, value, input], ...], access_list = [], nonce_key, nonce,
//!              valid_before, valid_after, fee_token, fee_payer_signature = "",
//!              tempo_authorization_list = [], signature])
//! ```
//!
//! `valid_before`, `valid_after` and `fee_token` are the empty string when absent; the
//! signature is the 65-byte `r || s || v` as an RLP byte string. The signing hash is
//! `keccak256(0x76 || rlp([...the same items without the signature]))`; with no fee payer
//! the fee token stays in. The transaction hash is `keccak256` of the full encoding.
//!
//! The decoder accepts only what the encoder can produce: no fee payer signature, no key
//! authorization, no access or authorization list entries, no contract creation, no
//! signature other than 65 bytes. Input is bounded to [`MAX_TX_BYTES`] and
//! [`MAX_CALLS`] calls.

use super::protocol::{MAX_RAW_TX_BYTES, TEMPO_TX_TYPE_ID};
use super::rlp::{self, Item, ListReader, RlpError};
use super::secp256k1::{self, SigError, SIGNATURE_LEN};
use super::tempo::keccak256;

/// Longest transaction the decoder reads.
pub const MAX_TX_BYTES: usize = MAX_RAW_TX_BYTES;
/// Most calls the decoder accepts.
pub const MAX_CALLS: usize = 16;
/// `nonce_key` of an expiring-nonce transaction: `2^256 - 1`.
pub const EXPIRING_NONCE_KEY: [u8; 32] = [0xff; 32];
/// How long an expiring-nonce transaction stays valid, in seconds.
pub const EXPIRING_NONCE_VALIDITY_SECS: u64 = 25;

/// One call of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub to: [u8; 20],
    pub value: u128,
    pub input: Vec<u8>,
}

/// A Tempo transaction without its signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempoTx {
    pub chain_id: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
    pub calls: Vec<Call>,
    /// `uint256`, big endian. [`EXPIRING_NONCE_KEY`] for an expiring nonce.
    pub nonce_key: [u8; 32],
    pub nonce: u64,
    /// Unix seconds. `Some(0)` is encoded as absent (the encoding of zero is the empty
    /// string), and the decoder never returns it.
    pub valid_before: Option<u64>,
    /// Unix seconds; same zero rule as `valid_before`.
    pub valid_after: Option<u64>,
    pub fee_token: Option<[u8; 20]>,
}

/// Why a transaction could not be built or read. A closed set, carrying no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TxError {
    /// Longer than [`MAX_TX_BYTES`].
    #[error("transaction too large")]
    TooLarge,
    /// Not canonical RLP in the Tempo layout.
    #[error("malformed transaction")]
    Malformed,
    /// Well formed, but uses a feature this crate does not support (fee payer, key
    /// authorization, access or authorization list, contract creation, too many calls).
    #[error("unsupported transaction feature")]
    Unsupported,
    /// The signature is not a 65-byte secp256k1 signature.
    #[error("unsupported signature")]
    UnsupportedSignature,
    /// `now + validity` overflows.
    #[error("validity window overflow")]
    TimeOverflow,
}

impl TxError {
    /// Closed code for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLarge => "tx_too_large",
            Self::Malformed => "tx_malformed",
            Self::Unsupported => "tx_unsupported",
            Self::UnsupportedSignature => "unsupported_signature",
            Self::TimeOverflow => "tx_time_overflow",
        }
    }
}

impl From<RlpError> for TxError {
    fn from(_: RlpError) -> Self {
        Self::Malformed
    }
}

impl TempoTx {
    /// Make this an expiring-nonce transaction valid for the next
    /// [`EXPIRING_NONCE_VALIDITY_SECS`] seconds from `now_unix`: `nonce_key = 2^256 - 1`,
    /// `nonce = 0`, `valid_before = now + 25`.
    pub fn set_expiring_nonce(&mut self, now_unix: u64) -> Result<(), TxError> {
        let valid_before = now_unix
            .checked_add(EXPIRING_NONCE_VALIDITY_SECS)
            .ok_or(TxError::TimeOverflow)?;
        self.nonce_key = EXPIRING_NONCE_KEY;
        self.nonce = 0;
        self.valid_before = Some(valid_before);
        Ok(())
    }

    /// Whether the nonce fields are those of an expiring-nonce transaction.
    pub fn has_expiring_nonce(&self) -> bool {
        self.nonce_key == EXPIRING_NONCE_KEY && self.nonce == 0 && self.valid_before.is_some()
    }

    /// The RLP items shared by the signing form and the signed form, concatenated.
    fn encode_items(&self) -> Vec<u8> {
        let mut calls = Vec::new();
        for call in &self.calls {
            let mut item = Vec::new();
            rlp::put_string(&mut item, &call.to);
            rlp::put_uint(&mut item, call.value);
            rlp::put_string(&mut item, &call.input);
            rlp::put_list(&mut calls, &item);
        }
        let mut out = Vec::new();
        rlp::put_uint(&mut out, u128::from(self.chain_id));
        rlp::put_uint(&mut out, self.max_priority_fee_per_gas);
        rlp::put_uint(&mut out, self.max_fee_per_gas);
        rlp::put_uint(&mut out, u128::from(self.gas_limit));
        rlp::put_list(&mut out, &calls);
        rlp::put_list(&mut out, &[]); // access list
        rlp::put_uint_be(&mut out, &self.nonce_key);
        rlp::put_uint(&mut out, u128::from(self.nonce));
        rlp::put_uint(&mut out, u128::from(self.valid_before.unwrap_or(0)));
        rlp::put_uint(&mut out, u128::from(self.valid_after.unwrap_or(0)));
        rlp::put_string(
            &mut out,
            self.fee_token
                .as_ref()
                .map_or(&[][..], |token| token.as_slice()),
        );
        rlp::put_string(&mut out, &[]); // fee payer signature
        rlp::put_list(&mut out, &[]); // tempo authorization list
        out
    }

    /// `0x76 || rlp([items…])`: the bytes the signing hash covers.
    pub fn signing_encoding(&self) -> Vec<u8> {
        let mut out = vec![TEMPO_TX_TYPE_ID];
        rlp::put_list(&mut out, &self.encode_items());
        out
    }

    /// `keccak256(signing_encoding)`: what the sender signs.
    pub fn signing_hash(&self) -> [u8; 32] {
        keccak256(&self.signing_encoding())
    }

    /// `0x76 || rlp([items…, signature])` with the signature as a byte string.
    pub fn serialize_signed(&self, signature: &[u8; SIGNATURE_LEN]) -> Vec<u8> {
        let mut payload = self.encode_items();
        rlp::put_string(&mut payload, signature);
        let mut out = vec![TEMPO_TX_TYPE_ID];
        rlp::put_list(&mut out, &payload);
        out
    }

    /// The address that produced `signature` over [`Self::signing_hash`].
    pub fn recover_sender(&self, signature: &[u8]) -> Result<[u8; 20], SigError> {
        secp256k1::recover_address(&self.signing_hash(), signature)
    }
}

/// The transaction hash of a serialized signed transaction.
pub fn tx_hash(serialized: &[u8]) -> [u8; 32] {
    keccak256(serialized)
}

/// The `expiringNonceHash` the precompile derives for an open sent by `sender`:
/// `keccak256(signing_encoding || sender)`.
pub fn expiring_nonce_hash(tx: &TempoTx, sender: &[u8; 20]) -> [u8; 32] {
    let mut buf = tx.signing_encoding();
    buf.extend_from_slice(sender);
    keccak256(&buf)
}

fn read_call(payload: &[u8]) -> Result<Call, TxError> {
    let mut call = ListReader::new(payload);
    let to: [u8; 20] = call
        .expect_string()?
        .try_into()
        // An empty `to` is a contract creation.
        .map_err(|_| TxError::Unsupported)?;
    let value = rlp::uint_u128(call.expect_string()?)?;
    let input = call.expect_string()?.to_vec();
    if !call.is_finished() {
        return Err(TxError::Malformed);
    }
    Ok(Call { to, value, input })
}

/// An optional `u64`: the empty string is `None`; a present value is canonical and
/// therefore non-zero.
fn optional_u64(bytes: &[u8]) -> Result<Option<u64>, TxError> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rlp::uint_u64(bytes)?))
    }
}

/// Read a signed `0x76` transaction. Refuses anything [`TempoTx::serialize_signed`] could
/// not have produced, so `serialize_signed(decode_signed(raw)) == raw` for every success.
pub fn decode_signed(raw: &[u8]) -> Result<(TempoTx, [u8; SIGNATURE_LEN]), TxError> {
    if raw.len() > MAX_TX_BYTES {
        return Err(TxError::TooLarge);
    }
    let body = match raw.split_first() {
        Some((&TEMPO_TX_TYPE_ID, body)) => body,
        _ => return Err(TxError::Malformed),
    };
    let (Item::List(fields), rest) = rlp::read_item(body)? else {
        return Err(TxError::Malformed);
    };
    if !rest.is_empty() {
        return Err(TxError::Malformed);
    }
    let mut fields = ListReader::new(fields);

    let chain_id = rlp::uint_u64(fields.expect_string()?)?;
    let max_priority_fee_per_gas = rlp::uint_u128(fields.expect_string()?)?;
    let max_fee_per_gas = rlp::uint_u128(fields.expect_string()?)?;
    let gas_limit = rlp::uint_u64(fields.expect_string()?)?;

    let mut call_list = ListReader::new(fields.expect_list()?);
    let mut calls = Vec::new();
    while let Some(item) = call_list.next_item()? {
        let Item::List(call) = item else {
            return Err(TxError::Malformed);
        };
        if calls.len() >= MAX_CALLS {
            return Err(TxError::Unsupported);
        }
        calls.push(read_call(call)?);
    }

    if !fields.expect_list()?.is_empty() {
        return Err(TxError::Unsupported); // access list
    }
    let nonce_key_bytes = fields.expect_string()?;
    if nonce_key_bytes.len() > 32 || nonce_key_bytes.first() == Some(&0) {
        return Err(TxError::Malformed);
    }
    let mut nonce_key = [0u8; 32];
    let offset = 32 - nonce_key_bytes.len();
    for (slot, byte) in nonce_key.iter_mut().skip(offset).zip(nonce_key_bytes) {
        *slot = *byte;
    }
    let nonce = rlp::uint_u64(fields.expect_string()?)?;
    let valid_before = optional_u64(fields.expect_string()?)?;
    let valid_after = optional_u64(fields.expect_string()?)?;
    let fee_token = match fields.expect_string()? {
        [] => None,
        token => Some(<[u8; 20]>::try_from(token).map_err(|_| TxError::Malformed)?),
    };
    if !fields.expect_string()?.is_empty() {
        return Err(TxError::Unsupported); // fee payer signature
    }
    if !fields.expect_list()?.is_empty() {
        return Err(TxError::Unsupported); // tempo authorization list
    }
    // After the authorization list comes either a key authorization (a list) or the
    // signature (a byte string).
    let signature = match fields.expect_item()? {
        Item::List(_) => return Err(TxError::Unsupported),
        Item::String(bytes) => {
            <[u8; SIGNATURE_LEN]>::try_from(bytes).map_err(|_| TxError::UnsupportedSignature)?
        }
    };
    if !fields.is_finished() {
        return Err(TxError::Malformed);
    }

    let tx = TempoTx {
        chain_id,
        max_priority_fee_per_gas,
        max_fee_per_gas,
        gas_limit,
        calls,
        nonce_key,
        nonce,
        valid_before,
        valid_after,
        fee_token,
    };
    Ok((tx, signature))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::abi::settle_calldata;
    use crate::mpp::session_vectors::Vectors;

    const U256_MAX_DECIMAL: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";

    fn open_tx(v: &Vectors) -> TempoTx {
        TempoTx {
            chain_id: v.chain_id(),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 600_000_001,
            gas_limit: 400_000,
            calls: vec![Call {
                to: v.escrow(),
                value: 0,
                input: v.bytes("open.calldata"),
            }],
            nonce_key: EXPIRING_NONCE_KEY,
            nonce: 0,
            valid_before: Some(1_800_000_000),
            valid_after: Some(1_700_000_000),
            fee_token: Some(v.fixed("token")),
        }
    }

    fn settle_tx(v: &Vectors) -> TempoTx {
        assert_eq!(v.text("settle.fields.nonceKey"), U256_MAX_DECIMAL);
        TempoTx {
            chain_id: v.chain_id(),
            max_priority_fee_per_gas: v.int("settle.fields.maxPriorityFeePerGas"),
            max_fee_per_gas: v.int("settle.fields.maxFeePerGas"),
            gas_limit: u64::try_from(v.int("settle.fields.gas")).unwrap(),
            calls: vec![Call {
                to: v.escrow(),
                value: 0,
                input: v.bytes("settle.calldata"),
            }],
            nonce_key: EXPIRING_NONCE_KEY,
            nonce: u64::try_from(v.int("settle.fields.nonce")).unwrap(),
            valid_before: Some(u64::try_from(v.int("settle.fields.validBefore")).unwrap()),
            valid_after: None,
            fee_token: Some(v.fixed("token")),
        }
    }

    #[test]
    fn the_open_vector_decodes_to_the_fields_it_was_built_from() {
        let v = Vectors::load();
        let raw = v.bytes("open.serialized");
        let (tx, signature) = decode_signed(&raw).unwrap();
        assert_eq!(tx, open_tx(&v));
        assert!(tx.has_expiring_nonce());
        assert_eq!(signature.len(), 65);
    }

    #[test]
    fn the_open_vector_round_trips_byte_for_byte() {
        let v = Vectors::load();
        let raw = v.bytes("open.serialized");
        let (tx, signature) = decode_signed(&raw).unwrap();
        assert_eq!(tx.serialize_signed(&signature), raw);
        assert_eq!(open_tx(&v).serialize_signed(&signature), raw);
    }

    #[test]
    fn the_open_vector_hashes_and_sender_match() {
        let v = Vectors::load();
        let raw = v.bytes("open.serialized");
        let (tx, signature) = decode_signed(&raw).unwrap();
        assert_eq!(tx.signing_hash(), v.fixed::<32>("open.signPayload"));
        assert_eq!(tx_hash(&raw), v.fixed::<32>("open.txHash"));
        let sender = tx.recover_sender(&signature).unwrap();
        assert_eq!(sender, v.fixed::<20>("payer"));
        assert_eq!(
            expiring_nonce_hash(&tx, &sender),
            v.fixed::<32>("open.expiringNonceHash")
        );
        assert_eq!(
            expiring_nonce_hash(&tx, &sender),
            v.fixed::<32>("descriptor.expiringNonceHash")
        );
        // The hash commits to the sender.
        assert_ne!(
            expiring_nonce_hash(&tx, &[0x01; 20]),
            v.fixed::<32>("open.expiringNonceHash")
        );
    }

    #[test]
    fn our_own_settle_transaction_matches_the_vector() {
        let v = Vectors::load();
        let d = v.descriptor();
        let calldata = settle_calldata(
            &d,
            v.int("voucher.cumulativeAmount"),
            &v.bytes("voucher.signature"),
        )
        .unwrap();
        assert_eq!(calldata, v.bytes("settle.calldata"));
        let tx = settle_tx(&v);
        assert_eq!(tx.signing_hash(), v.fixed::<32>("settle.signPayload"));

        let payee_key = v.secret_key("payeeKey");
        let signature = secp256k1::sign_digest(&payee_key, &tx.signing_hash()).unwrap();
        // RFC 6979 is deterministic, so the bytes are the vector's bytes.
        let serialized = tx.serialize_signed(&signature);
        assert_eq!(serialized, v.bytes("settle.serialized"));
        assert_eq!(tx_hash(&serialized), v.fixed::<32>("settle.txHash"));
        assert_eq!(tx.recover_sender(&signature), Ok(v.fixed::<20>("payee")));

        let (decoded, decoded_signature) = decode_signed(&serialized).unwrap();
        assert_eq!(decoded, tx);
        assert_eq!(decoded_signature, signature);
    }

    #[test]
    fn the_expiring_nonce_helper_sets_the_window() {
        let v = Vectors::load();
        let mut tx = settle_tx(&v);
        tx.nonce_key = [0; 32];
        tx.nonce = 9;
        tx.valid_before = None;
        assert!(!tx.has_expiring_nonce());
        tx.set_expiring_nonce(1_800_000_000).unwrap();
        assert_eq!(tx.nonce_key, [0xff; 32]);
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.valid_before, Some(1_800_000_025));
        assert!(tx.has_expiring_nonce());
        assert_eq!(tx.signing_hash(), v.fixed::<32>("settle.signPayload"));
        assert_eq!(tx.set_expiring_nonce(u64::MAX), Err(TxError::TimeOverflow));
    }

    #[test]
    fn the_signing_hash_covers_every_field() {
        let v = Vectors::load();
        let base = settle_tx(&v);
        let mut variants = Vec::new();
        for edit in 0..11 {
            let mut tx = base.clone();
            match edit {
                0 => tx.chain_id += 1,
                1 => tx.max_priority_fee_per_gas += 1,
                2 => tx.max_fee_per_gas += 1,
                3 => tx.gas_limit += 1,
                4 => tx.calls[0].value = 1,
                5 => tx.calls[0].input.push(0),
                6 => tx.nonce_key = [0; 32],
                7 => tx.nonce = 1,
                8 => tx.valid_before = Some(1),
                9 => tx.valid_after = Some(1),
                _ => tx.fee_token = None,
            }
            variants.push(tx.signing_hash());
        }
        let mut two_calls = base.clone();
        two_calls.calls.push(two_calls.calls[0].clone());
        variants.push(two_calls.signing_hash());
        for variant in &variants {
            assert_ne!(*variant, base.signing_hash());
        }
        let mut unique = variants.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), variants.len());
    }

    // -- decoder refusals ---------------------------------------------------------------

    /// The 14 top-level items of the open vector, each re-encoded.
    fn open_items() -> Vec<Vec<u8>> {
        let raw = Vectors::load().bytes("open.serialized");
        let (Item::List(payload), _) = rlp::read_item(&raw[1..]).unwrap() else {
            panic!("not a list");
        };
        let mut reader = ListReader::new(payload);
        let mut items = Vec::new();
        while let Some(item) = reader.next_item().unwrap() {
            let mut out = Vec::new();
            match item {
                Item::String(bytes) => rlp::put_string(&mut out, bytes),
                Item::List(bytes) => rlp::put_list(&mut out, bytes),
            }
            items.push(out);
        }
        assert_eq!(items.len(), 14);
        items
    }

    fn assemble(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![TEMPO_TX_TYPE_ID];
        rlp::put_list(&mut out, &items.concat());
        out
    }

    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        rlp::put_string(&mut out, bytes);
        out
    }

    #[test]
    fn reassembling_the_items_reproduces_the_vector() {
        let raw = Vectors::load().bytes("open.serialized");
        assert_eq!(assemble(&open_items()), raw);
        assert!(decode_signed(&raw).is_ok());
    }

    #[test]
    fn features_the_encoder_cannot_produce_are_refused() {
        // Item indices: 4 calls, 5 access list, 6 nonce key, 10 fee token,
        // 11 fee payer signature, 12 authorization list, 13 signature.
        let mut with_access = open_items();
        with_access[5] = {
            let mut out = Vec::new();
            rlp::put_list(&mut out, &[0xc0]);
            out
        };
        assert_eq!(
            decode_signed(&assemble(&with_access)),
            Err(TxError::Unsupported),
            "access list entry"
        );

        let mut fee_payer = open_items();
        fee_payer[11] = string(&[0x01; 65]);
        assert_eq!(
            decode_signed(&assemble(&fee_payer)),
            Err(TxError::Unsupported),
            "fee payer signature"
        );
        let mut fee_payer_list = open_items();
        let mut list = Vec::new();
        rlp::put_list(&mut list, &[0x01, 0x02, 0x03]);
        fee_payer_list[11] = list;
        assert!(decode_signed(&assemble(&fee_payer_list)).is_err());

        let mut auth = open_items();
        auth[12] = {
            let mut out = Vec::new();
            rlp::put_list(&mut out, &[0xc0]);
            out
        };
        assert_eq!(
            decode_signed(&assemble(&auth)),
            Err(TxError::Unsupported),
            "authorization list entry"
        );

        // A key authorization (a list) before the signature.
        let mut key_auth = open_items();
        let signature = key_auth.pop().unwrap();
        let mut list = Vec::new();
        rlp::put_list(&mut list, &[0x01]);
        key_auth.push(list);
        key_auth.push(signature);
        assert_eq!(
            decode_signed(&assemble(&key_auth)),
            Err(TxError::Unsupported),
            "key authorization"
        );
    }

    #[test]
    fn only_65_byte_signatures_are_accepted() {
        for len in [0usize, 1, 64, 66, 129] {
            let mut items = open_items();
            items[13] = string(&vec![0x22; len]);
            assert_eq!(
                decode_signed(&assemble(&items)),
                Err(TxError::UnsupportedSignature),
                "len {len}"
            );
        }
        // A signature that is a list.
        let mut items = open_items();
        items[13] = {
            let mut out = Vec::new();
            rlp::put_list(&mut out, &[0x01]);
            out
        };
        assert!(decode_signed(&assemble(&items)).is_err());
    }

    #[test]
    fn framing_type_and_size_are_enforced() {
        let raw = Vectors::load().bytes("open.serialized");
        assert_eq!(decode_signed(&[]), Err(TxError::Malformed));
        let mut wrong_type = raw.clone();
        wrong_type[0] = 0x02;
        assert_eq!(decode_signed(&wrong_type), Err(TxError::Malformed));
        let mut trailing = raw.clone();
        trailing.push(0);
        assert_eq!(decode_signed(&trailing), Err(TxError::Malformed));
        assert_eq!(
            decode_signed(&raw[..raw.len() - 1]),
            Err(TxError::Malformed)
        );
        let oversized = vec![0u8; MAX_TX_BYTES + 1];
        assert_eq!(decode_signed(&oversized), Err(TxError::TooLarge));
        // An item stream that is not a list.
        assert_eq!(
            decode_signed(&[TEMPO_TX_TYPE_ID, 0x80]),
            Err(TxError::Malformed)
        );
        // One item short, one item long.
        let mut short = open_items();
        short.pop();
        assert!(decode_signed(&assemble(&short)).is_err());
        let mut long = open_items();
        long.push(string(&[1]));
        assert_eq!(decode_signed(&assemble(&long)), Err(TxError::Malformed));
    }

    #[test]
    fn calls_are_bounded_and_must_have_a_target() {
        let v = Vectors::load();
        let mut tx = open_tx(&v);
        let signature = [0x33; 65];
        tx.calls = vec![tx.calls[0].clone(); MAX_CALLS];
        assert!(decode_signed(&tx.serialize_signed(&signature)).is_ok());
        tx.calls.push(tx.calls[0].clone());
        assert_eq!(
            decode_signed(&tx.serialize_signed(&signature)),
            Err(TxError::Unsupported)
        );
        // Contract creation (empty `to`) cannot be produced by the encoder.
        let mut items = open_items();
        items[4] = {
            let mut call = Vec::new();
            rlp::put_string(&mut call, &[]);
            rlp::put_uint(&mut call, 0);
            rlp::put_string(&mut call, &[1]);
            let mut calls = Vec::new();
            rlp::put_list(&mut calls, &call);
            let mut out = Vec::new();
            rlp::put_list(&mut out, &calls);
            out
        };
        assert_eq!(decode_signed(&assemble(&items)), Err(TxError::Unsupported));
        // A call with a short `to`, an extra field, or a non-list entry.
        for call_payload in [
            [string(&[1; 19]), string(&[]), string(&[])].concat(),
            [string(&[1; 20]), string(&[]), string(&[]), string(&[])].concat(),
        ] {
            let mut calls = Vec::new();
            let mut call = Vec::new();
            rlp::put_list(&mut call, &call_payload);
            calls.extend(call);
            let mut items = open_items();
            items[4] = {
                let mut out = Vec::new();
                rlp::put_list(&mut out, &calls);
                out
            };
            assert!(decode_signed(&assemble(&items)).is_err());
        }
        let mut items = open_items();
        items[4] = {
            let mut out = Vec::new();
            rlp::put_list(&mut out, &string(&[1]));
            out
        };
        assert_eq!(decode_signed(&assemble(&items)), Err(TxError::Malformed));
    }

    #[test]
    fn scalar_fields_must_be_canonical_and_in_range() {
        // chain id with a leading zero, a 9-byte gas limit, a 33-byte nonce key, a
        // 21-byte fee token, a fee token of 19 bytes.
        let cases: Vec<(usize, Vec<u8>)> = vec![
            (0, string(&[0x00, 0x01])),
            (3, string(&[1; 9])),
            (6, string(&[1; 33])),
            (6, string(&[0x00, 0x01])),
            (7, string(&[1; 9])),
            (8, string(&[1; 9])),
            (9, string(&[0x00, 0x01])),
            (10, string(&[1; 19])),
            (10, string(&[1; 21])),
            (1, string(&[1; 17])),
            (2, string(&[1; 17])),
        ];
        for (index, replacement) in cases {
            let mut items = open_items();
            items[index] = replacement;
            assert!(decode_signed(&assemble(&items)).is_err(), "item {index}");
        }
    }

    #[test]
    fn decode_then_serialize_is_the_identity_for_varied_transactions() {
        let v = Vectors::load();
        let signature = [0x44; 65];
        let mut variants = vec![open_tx(&v), settle_tx(&v)];
        let mut bare = open_tx(&v);
        bare.valid_before = None;
        bare.valid_after = None;
        bare.fee_token = None;
        bare.nonce_key = [0; 32];
        bare.nonce = u64::MAX;
        bare.max_fee_per_gas = u128::MAX;
        bare.calls[0].value = u128::MAX;
        bare.calls[0].input = vec![0xab; 300];
        variants.push(bare);
        let mut key = open_tx(&v);
        key.nonce_key[0] = 0;
        key.nonce_key[31] = 7;
        variants.push(key);
        for tx in variants {
            let raw = tx.serialize_signed(&signature);
            let (decoded, decoded_signature) = decode_signed(&raw).unwrap();
            assert_eq!(decoded, tx);
            assert_eq!(decoded_signature, signature);
            assert_eq!(decoded.serialize_signed(&signature), raw);
        }
    }
}
