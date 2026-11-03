//! Pre-broadcast check of a `transaction` payload.
//!
//! The server broadcasts the signed Tempo transaction a client sends
//! (`eth_sendRawTransactionSync`). Before it spends an RPC call, and before it relays
//! anything to the chain, it reads the transaction far enough to know that it pays this
//! challenge: the chain id is the configured one, and one of its calls is a
//! `transferWithMemo` of exactly the requested amount to the configured recipient on the
//! configured currency contract, with a memo bound to this realm and challenge id. A
//! transaction that does anything else is refused with no broadcast, so the endpoint is
//! not a relay for arbitrary transactions and a payment that would not be accepted never
//! leaves the payer's wallet.
//!
//! Layout (tempo-primitives `TempoTransaction::rlp_decode_fields`):
//! `0x76 || rlp([chain_id, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, calls,
//! access_list, nonce_key, nonce, valid_before, valid_after, fee_token,
//! fee_payer_signature, tempo_authorization_list, key_authorization?, signature…])`
//! with `calls = [[to, value, input], …]`. Only items 0 and 4 are read; the node validates
//! the rest, and the receipt is verified after the broadcast regardless.

use super::protocol::TEMPO_TX_TYPE_ID;
use super::rlp::{self, Item, ListReader, RlpError};
use super::tempo::is_bound_memo;

/// `transferWithMemo(address,uint256,bytes32)`: `keccak256(signature)[0..4]`.
pub const TRANSFER_WITH_MEMO_SELECTOR: [u8; 4] = [0x95, 0x77, 0x7d, 0x59];
/// Selector, then the recipient word, the amount word and the memo word.
const TRANSFER_WITH_MEMO_INPUT_LEN: usize = 4 + 32 * 3;

/// What a charge transaction must pay.
#[derive(Debug, Clone, Copy)]
pub struct PaymentTerms<'a> {
    pub chain_id: u64,
    pub currency: [u8; 20],
    pub recipient: [u8; 20],
    pub amount: u128,
    pub realm: &'a str,
    pub challenge_id: &'a str,
}

/// Why a transaction is not broadcast. A closed set, carrying no transaction content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransactionMismatch {
    /// Not a canonical Tempo transaction in the shape the reader needs.
    #[error("malformed transaction")]
    Malformed,
    /// Signed for another chain.
    #[error("wrong chain id")]
    ChainId,
    /// No call is the required `transferWithMemo`.
    #[error("no matching payment call")]
    NoPaymentCall,
}

impl TransactionMismatch {
    /// Closed code for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "tx_malformed",
            Self::ChainId => "tx_chain_id",
            Self::NoPaymentCall => "tx_no_payment_call",
        }
    }
}

impl From<RlpError> for TransactionMismatch {
    fn from(_: RlpError) -> Self {
        Self::Malformed
    }
}

/// Whether `raw` (a `0x76` transaction) pays `terms`.
pub fn check_payment_transaction(
    raw: &[u8],
    terms: &PaymentTerms<'_>,
) -> Result<(), TransactionMismatch> {
    let body = match raw.split_first() {
        Some((&TEMPO_TX_TYPE_ID, body)) => body,
        _ => return Err(TransactionMismatch::Malformed),
    };
    let (Item::List(fields), rest) = rlp::read_item(body)? else {
        return Err(TransactionMismatch::Malformed);
    };
    if !rest.is_empty() {
        return Err(TransactionMismatch::Malformed);
    }
    let mut fields = ListReader::new(fields);

    if rlp::uint_u64(fields.expect_string()?)? != terms.chain_id {
        return Err(TransactionMismatch::ChainId);
    }
    // max_priority_fee_per_gas, max_fee_per_gas, gas_limit.
    for _ in 0..3 {
        fields.expect_string()?;
    }
    let mut calls = ListReader::new(fields.expect_list()?);

    // Walk every call, so the whole list is known to be well formed, then require a payment.
    let mut pays = false;
    while let Some(call) = calls.next_item()? {
        let Item::List(call) = call else {
            return Err(TransactionMismatch::Malformed);
        };
        pays |= call_pays(call, terms)?;
    }
    if pays {
        Ok(())
    } else {
        Err(TransactionMismatch::NoPaymentCall)
    }
}

/// One call, `[to, value, input]`: is it the required `transferWithMemo`?
fn call_pays(call: &[u8], terms: &PaymentTerms<'_>) -> Result<bool, TransactionMismatch> {
    let mut call = ListReader::new(call);
    let to = call.expect_string()?;
    let value = call.expect_string()?;
    let input = call.expect_string()?;
    if !call.is_finished() {
        return Err(TransactionMismatch::Malformed);
    }
    // `to` is empty for a contract creation, else an address. A zero value is the empty string.
    Ok(to == terms.currency && value.is_empty() && input_pays(input, terms))
}

/// `transferWithMemo(recipient, amount, memo)` calldata for exactly these terms.
fn input_pays(input: &[u8], terms: &PaymentTerms<'_>) -> bool {
    if input.len() != TRANSFER_WITH_MEMO_INPUT_LEN {
        return false;
    }
    let (selector, args) = input.split_at(4);
    let (recipient_word, args) = args.split_at(32);
    let (amount_word, memo_word) = args.split_at(32);

    let (recipient_padding, recipient) = recipient_word.split_at(12);
    let (amount_padding, amount) = amount_word.split_at(16);
    let Ok(memo) = <[u8; 32]>::try_from(memo_word) else {
        return false;
    };
    selector == TRANSFER_WITH_MEMO_SELECTOR
        && recipient_padding.iter().all(|b| *b == 0)
        && recipient == terms.recipient
        && amount_padding.iter().all(|b| *b == 0)
        && amount == terms.amount.to_be_bytes()
        && is_bound_memo(&memo, terms.realm, terms.challenge_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::tempo::{attribution_memo, keccak256};
    use crate::mpp::testing::{
        payment_transaction, rlp_list, rlp_string, rlp_uint, tempo_transaction,
        transfer_with_memo_input, PaymentTx, TxCall,
    };

    const REALM: &str = "cloud-api.near.ai";
    const CHALLENGE: &str = "challenge-id-1";
    const CHAIN: u64 = 42431;
    const CURRENCY: [u8; 20] = [0x20; 20];
    const RECIPIENT: [u8; 20] = [0xaa; 20];
    const AMOUNT: u128 = 20_000;

    fn terms() -> PaymentTerms<'static> {
        PaymentTerms {
            chain_id: CHAIN,
            currency: CURRENCY,
            recipient: RECIPIENT,
            amount: AMOUNT,
            realm: REALM,
            challenge_id: CHALLENGE,
        }
    }

    fn paying() -> PaymentTx<'static> {
        PaymentTx {
            chain_id: CHAIN,
            currency: CURRENCY,
            recipient: RECIPIENT,
            amount: AMOUNT,
            realm: REALM,
            challenge_id: CHALLENGE,
            seed: 0,
        }
    }

    fn memo() -> [u8; 32] {
        attribution_memo(REALM, CHALLENGE)
    }

    fn call(input: Vec<u8>) -> TxCall {
        (CURRENCY, 0, input)
    }

    fn good_input() -> Vec<u8> {
        transfer_with_memo_input(RECIPIENT, AMOUNT, memo())
    }

    #[test]
    fn the_selector_is_transfer_with_memo() {
        assert_eq!(
            keccak256(b"transferWithMemo(address,uint256,bytes32)")[..4],
            TRANSFER_WITH_MEMO_SELECTOR
        );
    }

    #[test]
    fn an_mppx_shaped_charge_transaction_passes() {
        let raw = payment_transaction(&paying());
        assert_eq!(raw[0], 0x76);
        assert_eq!(check_payment_transaction(&raw, &terms()), Ok(()));
        // Another seed (another expiry) is another transaction and still pays.
        let other = payment_transaction(&PaymentTx {
            seed: 7,
            ..paying()
        });
        assert_ne!(raw, other);
        assert_eq!(check_payment_transaction(&other, &terms()), Ok(()));
    }

    #[test]
    fn a_payment_among_other_calls_passes_and_other_calls_alone_do_not() {
        let other_call: TxCall = ([0x77; 20], 5, vec![1, 2, 3]);
        let raw = tempo_transaction(CHAIN, &[other_call.clone(), call(good_input())], 0);
        assert_eq!(check_payment_transaction(&raw, &terms()), Ok(()));
        let raw = tempo_transaction(CHAIN, &[other_call], 0);
        assert_eq!(
            check_payment_transaction(&raw, &terms()),
            Err(TransactionMismatch::NoPaymentCall)
        );
        let raw = tempo_transaction(CHAIN, &[], 0);
        assert_eq!(
            check_payment_transaction(&raw, &terms()),
            Err(TransactionMismatch::NoPaymentCall)
        );
    }

    #[test]
    fn a_chain_other_than_the_configured_one_is_refused() {
        for chain in [1, CHAIN + 1, 0] {
            let raw = payment_transaction(&PaymentTx {
                chain_id: chain,
                ..paying()
            });
            assert_eq!(
                check_payment_transaction(&raw, &terms()),
                Err(TransactionMismatch::ChainId),
                "{chain}"
            );
        }
    }

    #[test]
    fn every_term_of_the_payment_is_checked() {
        let mut wrong_inputs: Vec<(&str, Vec<u8>)> = vec![
            (
                "recipient",
                transfer_with_memo_input([0xbb; 20], AMOUNT, memo()),
            ),
            (
                "amount below",
                transfer_with_memo_input(RECIPIENT, AMOUNT - 1, memo()),
            ),
            (
                "amount above",
                transfer_with_memo_input(RECIPIENT, AMOUNT + 1, memo()),
            ),
            (
                "memo for another challenge",
                transfer_with_memo_input(RECIPIENT, AMOUNT, attribution_memo(REALM, "other")),
            ),
            (
                "memo for another realm",
                transfer_with_memo_input(
                    RECIPIENT,
                    AMOUNT,
                    attribution_memo("evil.example", CHALLENGE),
                ),
            ),
            (
                "memo that is not an MPP memo",
                transfer_with_memo_input(RECIPIENT, AMOUNT, [0x11; 32]),
            ),
            (
                "no memo",
                transfer_with_memo_input(RECIPIENT, AMOUNT, [0; 32]),
            ),
        ];
        // Plain transfer(address,uint256) and an arbitrary selector.
        let mut transfer = vec![0xa9, 0x05, 0x9c, 0xbb];
        transfer.extend(good_input()[4..68].to_vec());
        wrong_inputs.push(("transfer()", transfer));
        let mut other_selector = good_input();
        other_selector[0] ^= 1;
        wrong_inputs.push(("selector", other_selector));
        // Dirty padding in the recipient and amount words.
        let mut dirty_recipient = good_input();
        dirty_recipient[4] = 1;
        wrong_inputs.push(("recipient padding", dirty_recipient));
        let mut dirty_amount = good_input();
        dirty_amount[36] = 1;
        wrong_inputs.push(("amount padding", dirty_amount));
        // Length.
        let mut long = good_input();
        long.push(0);
        wrong_inputs.push(("trailing calldata", long));
        wrong_inputs.push(("truncated", good_input()[..99].to_vec()));
        wrong_inputs.push(("empty", vec![]));

        for (label, input) in wrong_inputs {
            let raw = tempo_transaction(CHAIN, &[call(input)], 0);
            assert_eq!(
                check_payment_transaction(&raw, &terms()),
                Err(TransactionMismatch::NoPaymentCall),
                "{label}"
            );
        }

        // The call itself: another contract, a value, a creation (empty `to`).
        let wrong_calls: Vec<(&str, TxCall)> = vec![
            ("other contract", ([0x21; 20], 0, good_input())),
            ("value attached", (CURRENCY, 1, good_input())),
        ];
        for (label, wrong) in wrong_calls {
            let raw = tempo_transaction(CHAIN, &[wrong], 0);
            assert_eq!(
                check_payment_transaction(&raw, &terms()),
                Err(TransactionMismatch::NoPaymentCall),
                "{label}"
            );
        }
        let creation = rlp_list(&[rlp_string(&[]), rlp_uint(0), rlp_string(&good_input())]);
        let raw = with_raw_calls(&[creation]);
        assert_eq!(
            check_payment_transaction(&raw, &terms()),
            Err(TransactionMismatch::NoPaymentCall),
            "contract creation"
        );
    }

    /// A transaction whose `calls` are the given pre-encoded items.
    fn with_raw_calls(calls: &[Vec<u8>]) -> Vec<u8> {
        let fields = [
            rlp_uint(u128::from(CHAIN)),
            rlp_uint(1),
            rlp_uint(2),
            rlp_uint(3),
            rlp_list(calls),
            rlp_list(&[]),
            rlp_string(&[0xff; 32]),
            rlp_uint(0),
            rlp_uint(1),
            rlp_uint(0),
            rlp_string(&[]),
            rlp_uint(0),
            rlp_list(&[]),
            rlp_string(&[0x5a; 65]),
        ];
        let mut raw = vec![0x76];
        raw.extend(rlp_list(&fields));
        raw
    }

    #[test]
    fn malformed_transactions_are_refused_not_panicked_on() {
        let good = payment_transaction(&paying());
        let mut cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("type byte only", vec![0x76]),
            ("wrong type byte", [vec![0x02], good[1..].to_vec()].concat()),
            (
                "a string, not a list",
                [vec![0x76], rlp_string(&[1, 2, 3])].concat(),
            ),
            ("trailing bytes", [good.clone(), vec![0]].concat()),
            ("empty list", vec![0x76, 0xc0]),
            ("chain id with a leading zero", {
                let mut fields = vec![rlp_string(&[0, 1])];
                fields.push(rlp_uint(1));
                [vec![0x76], rlp_list(&fields)].concat()
            }),
            ("chain id wider than u64", {
                [vec![0x76], rlp_list(&[rlp_string(&[1; 9])])].concat()
            }),
            ("list where the chain id belongs", {
                [vec![0x76], rlp_list(&[rlp_list(&[])])].concat()
            }),
            ("calls is a string", {
                let fields = [
                    rlp_uint(u128::from(CHAIN)),
                    rlp_uint(1),
                    rlp_uint(2),
                    rlp_uint(3),
                    rlp_string(&good_input()),
                ];
                [vec![0x76], rlp_list(&fields)].concat()
            }),
            ("calls missing", {
                let fields = [rlp_uint(u128::from(CHAIN)), rlp_uint(1)];
                [vec![0x76], rlp_list(&fields)].concat()
            }),
            (
                "a call that is a string",
                with_raw_calls(&[rlp_string(&good_input())]),
            ),
            (
                "a call of four items",
                with_raw_calls(&[rlp_list(&[
                    rlp_string(&CURRENCY),
                    rlp_uint(0),
                    rlp_string(&good_input()),
                    rlp_uint(0),
                ])]),
            ),
            (
                "a call of two items",
                with_raw_calls(&[rlp_list(&[rlp_string(&CURRENCY), rlp_uint(0)])]),
            ),
            (
                "a call whose input is a list",
                with_raw_calls(&[rlp_list(&[
                    rlp_string(&CURRENCY),
                    rlp_uint(0),
                    rlp_list(&[]),
                ])]),
            ),
            (
                "a malformed call after a paying one",
                with_raw_calls(&[
                    rlp_list(&[
                        rlp_string(&CURRENCY),
                        rlp_uint(0),
                        rlp_string(&good_input()),
                    ]),
                    vec![0xc8, 0x01],
                ]),
            ),
        ];
        // Oversized declared lengths: the whole transaction, then a field, then a call input.
        cases.push((
            "outer list claims more than exists",
            vec![0x76, 0xf9, 0xff, 0xff, 0x01, 0x02],
        ));
        cases.push((
            "outer list length overflows",
            [vec![0x76, 0xff], vec![0xff; 8]].concat(),
        ));
        cases.push((
            "field claims 2^63 bytes",
            [vec![0x76, 0xca, 0xbf, 0x80], vec![0; 7], vec![0x01]].concat(),
        ));
        let mut bad_input_len = good[..good.len() / 2].to_vec();
        bad_input_len.extend([0xb9, 0xff, 0xff]);
        cases.push(("input claims 65535 bytes", bad_input_len));
        // Every strict prefix of a good transaction is refused.
        for end in 0..good.len() {
            assert!(
                check_payment_transaction(&good[..end], &terms()).is_err(),
                "prefix {end}"
            );
        }
        for (label, raw) in cases {
            assert!(
                check_payment_transaction(&raw, &terms()).is_err(),
                "{label}"
            );
        }
    }

    #[test]
    fn a_payload_of_the_maximum_size_is_walked_in_bounded_time() {
        // About 16 KiB of minimal, well-formed, non-paying calls (`[to, value, input]` with
        // all three empty): linear, no recursion, and the verdict is a plain refusal.
        let empty_call = rlp_list(&[rlp_string(&[]), rlp_uint(0), rlp_string(&[])]);
        let calls = vec![empty_call; crate::mpp::protocol::MAX_RAW_TX_BYTES / 4 - 100];
        let raw = with_raw_calls(&calls);
        assert!(raw.len() <= crate::mpp::protocol::MAX_RAW_TX_BYTES);
        assert_eq!(
            check_payment_transaction(&raw, &terms()),
            Err(TransactionMismatch::NoPaymentCall)
        );
    }
}
