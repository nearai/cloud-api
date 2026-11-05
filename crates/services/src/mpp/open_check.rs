//! Pre-broadcast check of a client's channel-open transaction.
//!
//! The server broadcasts the signed Tempo transaction a client sends. Before it spends an
//! RPC call, [`verify_open_transaction`] reads the transaction far enough to know that it
//! opens a channel this server will serve: one call, to the escrow precompile, `open(...)`
//! with the server as payee, no operator, the configured token and a non-zero deposit,
//! on the configured chain, with an expiring nonce. It recovers the sender and computes the
//! `expiringNonceHash`, and returns the channel descriptor and id the chain will derive, so
//! the caller can compare them to what the credential claims.

use super::abi::{self, ChannelDescriptor, OPEN_SELECTOR};
use super::channel::{channel_id, voucher_signer};
use super::secp256k1::SigError;
use super::tempo_tx::{self, expiring_nonce_hash, TxError};

/// What an open must open: the deployment and the server-side terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenTerms {
    /// The channel reserve precompile.
    pub escrow: [u8; 20],
    pub chain_id: u64,
    /// The server's settlement signer: only the payee can settle and close.
    pub payee: [u8; 20],
    pub token: [u8; 20],
}

/// An open transaction that passed every check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedOpen {
    pub descriptor: ChannelDescriptor,
    pub channel_id: [u8; 32],
    /// Who must sign vouchers: `authorized_signer`, or the payer when that is zero.
    pub voucher_signer: [u8; 20],
    pub deposit: u128,
    /// The recovered transaction sender, which is the descriptor's payer.
    pub sender: [u8; 20],
}

/// Why an open is not broadcast. A closed set, carrying no transaction content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenMismatch {
    /// Not a canonical Tempo transaction in the supported shape.
    #[error("malformed transaction")]
    Malformed,
    /// The signature is not a 65-byte secp256k1 signature.
    #[error("unsupported signature")]
    UnsupportedSignature,
    /// Signed for another chain.
    #[error("wrong chain id")]
    ChainId,
    /// Not exactly one call.
    #[error("not exactly one call")]
    CallCount,
    /// The call does not target the escrow.
    #[error("call is not to the escrow")]
    Target,
    /// The call carries native value.
    #[error("call carries value")]
    Value,
    /// The call is not `open(...)`.
    #[error("not an open call")]
    Selector,
    /// The `open(...)` arguments are not canonical ABI.
    #[error("malformed open calldata")]
    Calldata,
    /// The operator is not the zero address.
    #[error("operator is set")]
    Operator,
    /// The payee is not the server.
    #[error("wrong payee")]
    Payee,
    /// The token is not the configured currency.
    #[error("wrong token")]
    Token,
    /// The deposit is zero.
    #[error("zero deposit")]
    Deposit,
    /// The transaction does not use an expiring nonce.
    #[error("not an expiring-nonce transaction")]
    ExpiringNonce,
    /// The sender cannot be recovered from the signature.
    #[error("invalid signature")]
    Signature,
}

impl OpenMismatch {
    /// Closed code for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "open_malformed",
            Self::UnsupportedSignature => "unsupported_signature",
            Self::ChainId => "open_chain_id",
            Self::CallCount => "open_call_count",
            Self::Target => "open_target",
            Self::Value => "open_value",
            Self::Selector => "open_selector",
            Self::Calldata => "open_calldata",
            Self::Operator => "open_operator",
            Self::Payee => "open_payee",
            Self::Token => "open_token",
            Self::Deposit => "open_deposit",
            Self::ExpiringNonce => "open_expiring_nonce",
            Self::Signature => "open_signature",
        }
    }
}

impl From<TxError> for OpenMismatch {
    fn from(error: TxError) -> Self {
        match error {
            TxError::UnsupportedSignature => Self::UnsupportedSignature,
            _ => Self::Malformed,
        }
    }
}

impl From<SigError> for OpenMismatch {
    fn from(error: SigError) -> Self {
        match error {
            SigError::UnsupportedSignature => Self::UnsupportedSignature,
            _ => Self::Signature,
        }
    }
}

/// Check the signed transaction `raw` against `expected` and derive the channel it opens.
pub fn verify_open_transaction(
    raw: &[u8],
    expected: &OpenTerms,
) -> Result<VerifiedOpen, OpenMismatch> {
    let (tx, signature) = tempo_tx::decode_signed(raw)?;
    if tx.chain_id != expected.chain_id {
        return Err(OpenMismatch::ChainId);
    }
    let [call] = tx.calls.as_slice() else {
        return Err(OpenMismatch::CallCount);
    };
    if call.to != expected.escrow {
        return Err(OpenMismatch::Target);
    }
    if call.value != 0 {
        return Err(OpenMismatch::Value);
    }
    if call.input.get(..4) != Some(&OPEN_SELECTOR[..]) {
        return Err(OpenMismatch::Selector);
    }
    let args = abi::decode_open_calldata(&call.input).map_err(|_| OpenMismatch::Calldata)?;
    if args.operator != [0u8; 20] {
        return Err(OpenMismatch::Operator);
    }
    if args.payee != expected.payee {
        return Err(OpenMismatch::Payee);
    }
    if args.token != expected.token {
        return Err(OpenMismatch::Token);
    }
    if args.deposit == 0 {
        return Err(OpenMismatch::Deposit);
    }
    if !tx.has_expiring_nonce() {
        return Err(OpenMismatch::ExpiringNonce);
    }
    let sender = tx.recover_sender(&signature)?;
    let descriptor = ChannelDescriptor {
        payer: sender,
        payee: args.payee,
        operator: args.operator,
        token: args.token,
        salt: args.salt,
        authorized_signer: args.authorized_signer,
        expiring_nonce_hash: expiring_nonce_hash(&tx, &sender),
    };
    Ok(VerifiedOpen {
        descriptor,
        channel_id: channel_id(&descriptor, &expected.escrow, expected.chain_id),
        voucher_signer: voucher_signer(&descriptor),
        deposit: args.deposit,
        sender,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::abi::{open_calldata, settle_calldata};
    use crate::mpp::secp256k1::{recover_address, sign_digest};
    use crate::mpp::session_vectors::Vectors;
    use crate::mpp::tempo_tx::{decode_signed, Call, TempoTx};

    fn terms(v: &Vectors) -> OpenTerms {
        OpenTerms {
            escrow: v.escrow(),
            chain_id: v.chain_id(),
            payee: v.fixed("payee"),
            token: v.fixed("token"),
        }
    }

    /// The vector's open transaction, changed by `edit` and re-signed by the payer.
    fn resigned(v: &Vectors, edit: impl FnOnce(&mut TempoTx)) -> Vec<u8> {
        let (mut tx, _) = decode_signed(&v.bytes("open.serialized")).unwrap();
        edit(&mut tx);
        let signature = sign_digest(&v.secret_key("payerKey"), &tx.signing_hash()).unwrap();
        tx.serialize_signed(&signature)
    }

    fn open_call(v: &Vectors, payee: [u8; 20], token: [u8; 20], deposit: u128) -> Vec<u8> {
        open_calldata(
            &payee,
            &[0; 20],
            &token,
            deposit,
            &v.fixed("salt"),
            &[0; 20],
        )
        .unwrap()
    }

    #[test]
    fn the_vector_opens_the_vector_channel() {
        let v = Vectors::load();
        let verified = verify_open_transaction(&v.bytes("open.serialized"), &terms(&v)).unwrap();
        assert_eq!(verified.descriptor, v.descriptor());
        assert_eq!(verified.channel_id, v.fixed::<32>("channelId"));
        assert_eq!(verified.sender, v.fixed::<20>("payer"));
        assert_eq!(verified.voucher_signer, v.fixed::<20>("payer"));
        assert_eq!(verified.deposit, v.int("deposit"));
    }

    #[test]
    fn the_vector_voucher_and_close_signatures_belong_to_the_voucher_signer() {
        let v = Vectors::load();
        let verified = verify_open_transaction(&v.bytes("open.serialized"), &terms(&v)).unwrap();
        for (digest, signature) in [
            ("voucher.digest", "voucher.signature"),
            ("closeAuthorization.digest", "closeAuthorization.signature"),
        ] {
            assert_eq!(
                recover_address(&v.fixed::<32>(digest), &v.bytes(signature)),
                Ok(verified.voucher_signer)
            );
        }
    }

    #[test]
    fn a_wrong_payee_token_or_chain_is_refused() {
        let v = Vectors::load();
        let raw = v.bytes("open.serialized");
        let mut other_payee = terms(&v);
        other_payee.payee[19] ^= 1;
        assert_eq!(
            verify_open_transaction(&raw, &other_payee),
            Err(OpenMismatch::Payee)
        );
        let mut other_token = terms(&v);
        other_token.token[19] ^= 1;
        assert_eq!(
            verify_open_transaction(&raw, &other_token),
            Err(OpenMismatch::Token)
        );
        let mut other_chain = terms(&v);
        other_chain.chain_id += 1;
        assert_eq!(
            verify_open_transaction(&raw, &other_chain),
            Err(OpenMismatch::ChainId)
        );
        let mut other_escrow = terms(&v);
        other_escrow.escrow[19] ^= 1;
        assert_eq!(
            verify_open_transaction(&raw, &other_escrow),
            Err(OpenMismatch::Target)
        );
    }

    #[test]
    fn the_call_must_be_exactly_one_open_to_the_escrow() {
        let v = Vectors::load();
        let terms = terms(&v);
        let call = |tx: &TempoTx| tx.calls[0].clone();

        let extra = resigned(&v, |tx| tx.calls.push(call(tx)));
        assert_eq!(
            verify_open_transaction(&extra, &terms),
            Err(OpenMismatch::CallCount)
        );
        let none = resigned(&v, |tx| tx.calls.clear());
        assert_eq!(
            verify_open_transaction(&none, &terms),
            Err(OpenMismatch::CallCount)
        );
        // An unrelated call ahead of the open (as an approve would be).
        let prefixed = resigned(&v, |tx| {
            tx.calls.insert(
                0,
                Call {
                    to: terms.token,
                    value: 0,
                    input: vec![0x09, 0x5e, 0xa7, 0xb3],
                },
            );
        });
        assert_eq!(
            verify_open_transaction(&prefixed, &terms),
            Err(OpenMismatch::CallCount)
        );

        let settle = settle_calldata(&v.descriptor(), 1, &[0u8; 65]).unwrap();
        let wrong_selector = resigned(&v, |tx| tx.calls[0].input = settle);
        assert_eq!(
            verify_open_transaction(&wrong_selector, &terms),
            Err(OpenMismatch::Selector)
        );
        let empty_input = resigned(&v, |tx| tx.calls[0].input.clear());
        assert_eq!(
            verify_open_transaction(&empty_input, &terms),
            Err(OpenMismatch::Selector)
        );
        let truncated = resigned(&v, |tx| {
            tx.calls[0].input.pop();
        });
        assert_eq!(
            verify_open_transaction(&truncated, &terms),
            Err(OpenMismatch::Calldata)
        );
        let elsewhere = resigned(&v, |tx| tx.calls[0].to = terms.token);
        assert_eq!(
            verify_open_transaction(&elsewhere, &terms),
            Err(OpenMismatch::Target)
        );
        let with_value = resigned(&v, |tx| tx.calls[0].value = 1);
        assert_eq!(
            verify_open_transaction(&with_value, &terms),
            Err(OpenMismatch::Value)
        );
    }

    #[test]
    fn operator_deposit_and_nonce_rules_are_enforced() {
        let v = Vectors::load();
        let terms = terms(&v);
        let with_operator = resigned(&v, |tx| {
            tx.calls[0].input = open_calldata(
                &terms.payee,
                &[0x77; 20],
                &terms.token,
                1,
                &[1; 32],
                &[0; 20],
            )
            .unwrap();
        });
        assert_eq!(
            verify_open_transaction(&with_operator, &terms),
            Err(OpenMismatch::Operator)
        );
        let zero_deposit = resigned(&v, |tx| {
            tx.calls[0].input = open_call(&v, terms.payee, terms.token, 0);
        });
        assert_eq!(
            verify_open_transaction(&zero_deposit, &terms),
            Err(OpenMismatch::Deposit)
        );
        let sequential = resigned(&v, |tx| {
            tx.nonce_key = [0; 32];
            tx.nonce = 5;
        });
        assert_eq!(
            verify_open_transaction(&sequential, &terms),
            Err(OpenMismatch::ExpiringNonce)
        );
        let unbounded = resigned(&v, |tx| tx.valid_before = None);
        assert_eq!(
            verify_open_transaction(&unbounded, &terms),
            Err(OpenMismatch::ExpiringNonce)
        );
    }

    #[test]
    fn a_delegated_signer_becomes_the_voucher_signer() {
        let v = Vectors::load();
        let terms = terms(&v);
        let delegate = [0x5a; 20];
        let raw = resigned(&v, |tx| {
            tx.calls[0].input =
                open_calldata(&terms.payee, &[0; 20], &terms.token, 7, &[1; 32], &delegate)
                    .unwrap();
        });
        let verified = verify_open_transaction(&raw, &terms).unwrap();
        assert_eq!(verified.voucher_signer, delegate);
        assert_eq!(verified.descriptor.payer, v.fixed::<20>("payer"));
        assert_eq!(verified.deposit, 7);
    }

    #[test]
    fn a_tampered_or_unsupported_signature_never_yields_the_payer() {
        let v = Vectors::load();
        let terms = terms(&v);
        let raw = v.bytes("open.serialized");
        let payer = v.fixed::<20>("payer");
        // The signature is the last 65 bytes of the transaction.
        for index in [raw.len() - 65, raw.len() - 40, raw.len() - 2] {
            let mut tampered = raw.clone();
            tampered[index] ^= 0x01;
            match verify_open_transaction(&tampered, &terms) {
                Ok(verified) => {
                    assert_ne!(verified.sender, payer, "byte {index}");
                    assert_ne!(verified.channel_id, v.fixed::<32>("channelId"));
                }
                Err(error) => assert!(
                    matches!(error, OpenMismatch::Signature),
                    "byte {index}: {error}"
                ),
            }
        }
        // v out of range.
        let mut bad_v = raw.clone();
        let last = bad_v.len() - 1;
        bad_v[last] = 9;
        assert_eq!(
            verify_open_transaction(&bad_v, &terms),
            Err(OpenMismatch::Signature)
        );
        // A 64-byte signature cannot be a secp256k1 recoverable signature.
        let (tx, signature) = decode_signed(&raw).unwrap();
        let mut items = tx.serialize_signed(&signature);
        items.truncate(items.len() - 1);
        assert!(verify_open_transaction(&items, &terms).is_err());
    }

    #[test]
    fn garbage_is_malformed() {
        let v = Vectors::load();
        let terms = terms(&v);
        assert_eq!(
            verify_open_transaction(&[], &terms),
            Err(OpenMismatch::Malformed)
        );
        assert_eq!(
            verify_open_transaction(&[0x76, 0xc0], &terms),
            Err(OpenMismatch::Malformed)
        );
        assert_eq!(
            verify_open_transaction(&vec![0x76; 20_000], &terms),
            Err(OpenMismatch::Malformed)
        );
    }
}
