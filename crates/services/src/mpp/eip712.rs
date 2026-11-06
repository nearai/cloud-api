//! EIP-712 digests of the TIP-1034 channel reserve precompile: `Voucher` and
//! `CloseAuthorization`.
//!
//! Domain: `{name: "TIP20 Channel Reserve", version: "1", chainId, verifyingContract: escrow}`.
//! Both structs are `(bytes32 channelId, uint96 cumulativeAmount)`; the amount is hashed
//! as a `uint256` word. Digest = `keccak256(0x1901 || domainSeparator || structHash)`.
//! Amounts above `2^96 - 1` are refused: the precompile's `uint96` cannot hold them, and
//! a digest for such an amount would be a voucher the chain can never honour.

use super::abi::{address_word, uint_word};
use super::tempo::keccak256;

/// Largest `uint96`.
pub const MAX_UINT96: u128 = (1u128 << 96) - 1;

/// EIP-712 domain name of the precompile.
pub const DOMAIN_NAME: &str = "TIP20 Channel Reserve";
/// EIP-712 domain version of the precompile.
pub const DOMAIN_VERSION: &str = "1";

/// `keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")`.
pub const DOMAIN_TYPEHASH: [u8; 32] = [
    0x8b, 0x73, 0xc3, 0xc6, 0x9b, 0xb8, 0xfe, 0x3d, 0x51, 0x2e, 0xcc, 0x4c, 0xf7, 0x59, 0xcc, 0x79,
    0x23, 0x9f, 0x7b, 0x17, 0x9b, 0x0f, 0xfa, 0xca, 0xa9, 0xa7, 0x5d, 0x52, 0x2b, 0x39, 0x40, 0x0f,
];
/// `keccak256("Voucher(bytes32 channelId,uint96 cumulativeAmount)")`.
pub const VOUCHER_TYPEHASH: [u8; 32] = [
    0x47, 0x30, 0xb4, 0xb2, 0xfe, 0x8d, 0xe5, 0x22, 0x47, 0x5e, 0x80, 0xe0, 0x10, 0x87, 0x7c, 0x5b,
    0x58, 0xad, 0x10, 0xeb, 0x07, 0xfd, 0x04, 0x36, 0x61, 0x5e, 0xe1, 0x23, 0xf7, 0x43, 0xbb, 0xe8,
];
/// `keccak256("CloseAuthorization(bytes32 channelId,uint96 cumulativeAmount)")`.
pub const CLOSE_AUTHORIZATION_TYPEHASH: [u8; 32] = [
    0x15, 0x58, 0x8a, 0x02, 0xd9, 0xb0, 0x68, 0xec, 0x4a, 0x45, 0x93, 0xa3, 0xd3, 0xbd, 0x10, 0x68,
    0xf3, 0x61, 0x4e, 0x6c, 0x28, 0xe0, 0x13, 0x50, 0x0c, 0xe5, 0x54, 0x84, 0x6f, 0x4b, 0x46, 0x46,
];

/// The cumulative amount does not fit a `uint96`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("amount exceeds uint96")]
pub struct AmountOutOfRange;

/// The EIP-712 domain separator of the precompile at `escrow` on `chain_id`.
pub fn domain_separator(escrow: &[u8; 20], chain_id: u64) -> [u8; 32] {
    let mut buf = Vec::with_capacity(32 * 5);
    buf.extend_from_slice(&DOMAIN_TYPEHASH);
    buf.extend_from_slice(&keccak256(DOMAIN_NAME.as_bytes()));
    buf.extend_from_slice(&keccak256(DOMAIN_VERSION.as_bytes()));
    buf.extend_from_slice(&uint_word(u128::from(chain_id)));
    buf.extend_from_slice(&address_word(escrow));
    keccak256(&buf)
}

fn typed_digest(
    typehash: &[u8; 32],
    escrow: &[u8; 20],
    chain_id: u64,
    channel_id: &[u8; 32],
    cumulative: u128,
) -> Result<[u8; 32], AmountOutOfRange> {
    if cumulative > MAX_UINT96 {
        return Err(AmountOutOfRange);
    }
    let mut struct_buf = Vec::with_capacity(32 * 3);
    struct_buf.extend_from_slice(typehash);
    struct_buf.extend_from_slice(channel_id);
    struct_buf.extend_from_slice(&uint_word(cumulative));
    let mut buf = Vec::with_capacity(2 + 32 * 2);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&domain_separator(escrow, chain_id));
    buf.extend_from_slice(&keccak256(&struct_buf));
    Ok(keccak256(&buf))
}

/// The digest a voucher signer signs for `cumulative` on `channel_id`.
pub fn voucher_digest(
    escrow: &[u8; 20],
    chain_id: u64,
    channel_id: &[u8; 32],
    cumulative: u128,
) -> Result<[u8; 32], AmountOutOfRange> {
    typed_digest(&VOUCHER_TYPEHASH, escrow, chain_id, channel_id, cumulative)
}

/// The digest a payer (or voucher signer) signs to authorize a cooperative close at
/// `cumulative` on `channel_id`.
pub fn close_authorization_digest(
    escrow: &[u8; 20],
    chain_id: u64,
    channel_id: &[u8; 32],
    cumulative: u128,
) -> Result<[u8; 32], AmountOutOfRange> {
    typed_digest(
        &CLOSE_AUTHORIZATION_TYPEHASH,
        escrow,
        chain_id,
        channel_id,
        cumulative,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::session_vectors::Vectors;

    #[test]
    fn typehash_constants_are_the_hashes_of_their_type_strings() {
        assert_eq!(
            DOMAIN_TYPEHASH,
            keccak256(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
            )
        );
        assert_eq!(
            VOUCHER_TYPEHASH,
            keccak256(b"Voucher(bytes32 channelId,uint96 cumulativeAmount)")
        );
        assert_eq!(
            CLOSE_AUTHORIZATION_TYPEHASH,
            keccak256(b"CloseAuthorization(bytes32 channelId,uint96 cumulativeAmount)")
        );
    }

    #[test]
    fn typehashes_match_the_vectors() {
        let v = Vectors::load();
        assert_eq!(VOUCHER_TYPEHASH, v.fixed::<32>("voucherTypehash"));
        assert_eq!(
            CLOSE_AUTHORIZATION_TYPEHASH,
            v.fixed::<32>("closeAuthorizationTypehash")
        );
    }

    #[test]
    fn domain_separator_matches_the_vector() {
        let v = Vectors::load();
        assert_eq!(
            domain_separator(&v.escrow(), v.chain_id()),
            v.fixed::<32>("domainSeparator")
        );
        // Another chain or another escrow is another domain.
        assert_ne!(
            domain_separator(&v.escrow(), v.chain_id() + 1),
            v.fixed::<32>("domainSeparator")
        );
        assert_ne!(
            domain_separator(&[0x11; 20], v.chain_id()),
            v.fixed::<32>("domainSeparator")
        );
    }

    #[test]
    fn voucher_and_close_digests_match_the_vectors() {
        let v = Vectors::load();
        let channel_id = v.fixed::<32>("channelId");
        let amount = v.int("voucher.cumulativeAmount");
        assert_eq!(
            voucher_digest(&v.escrow(), v.chain_id(), &channel_id, amount),
            Ok(v.fixed::<32>("voucher.digest"))
        );
        assert_eq!(
            close_authorization_digest(&v.escrow(), v.chain_id(), &channel_id, amount),
            Ok(v.fixed::<32>("closeAuthorization.digest"))
        );
        // Voucher and close authorization are different structs: never the same digest.
        assert_ne!(
            v.fixed::<32>("voucher.digest"),
            v.fixed::<32>("closeAuthorization.digest")
        );
    }

    #[test]
    fn the_digest_binds_every_input() {
        let v = Vectors::load();
        let channel_id = v.fixed::<32>("channelId");
        let base = voucher_digest(&v.escrow(), v.chain_id(), &channel_id, 1_250_000).unwrap();
        let mut other_channel = channel_id;
        other_channel[0] ^= 1;
        for changed in [
            voucher_digest(&v.escrow(), v.chain_id(), &channel_id, 1_250_001).unwrap(),
            voucher_digest(&v.escrow(), v.chain_id(), &other_channel, 1_250_000).unwrap(),
            voucher_digest(&v.escrow(), v.chain_id() + 1, &channel_id, 1_250_000).unwrap(),
            voucher_digest(&[0x22; 20], v.chain_id(), &channel_id, 1_250_000).unwrap(),
        ] {
            assert_ne!(changed, base);
        }
    }

    #[test]
    fn amounts_above_uint96_are_refused() {
        let v = Vectors::load();
        let channel_id = v.fixed::<32>("channelId");
        assert!(voucher_digest(&v.escrow(), v.chain_id(), &channel_id, MAX_UINT96).is_ok());
        assert_eq!(
            voucher_digest(&v.escrow(), v.chain_id(), &channel_id, MAX_UINT96 + 1),
            Err(AmountOutOfRange)
        );
        assert_eq!(
            close_authorization_digest(&v.escrow(), v.chain_id(), &channel_id, MAX_UINT96 + 1),
            Err(AmountOutOfRange)
        );
        assert_eq!(
            close_authorization_digest(&v.escrow(), v.chain_id(), &channel_id, u128::MAX),
            Err(AmountOutOfRange)
        );
    }
}
