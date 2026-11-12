//! TIP-1034 channel identity: the channel id and the voucher signer.
//!
//! `channelId = keccak256(abi.encode(payer, payee, operator, token, salt, authorizedSigner,
//! expiringNonceHash, escrow, uint256 chainId))`: nine 32-byte words in exactly that order
//! (addresses left padded). The id commits to the escrow contract and the chain, so a
//! voucher for one deployment can never be replayed on another.

use super::abi::{address_word, uint_word, ChannelDescriptor};
use super::tempo::keccak256;

/// The id of the channel `descriptor` opens on the precompile at `escrow` on `chain_id`.
pub fn channel_id(descriptor: &ChannelDescriptor, escrow: &[u8; 20], chain_id: u64) -> [u8; 32] {
    let mut words = Vec::with_capacity(9 * 32);
    words.extend_from_slice(&address_word(&descriptor.payer));
    words.extend_from_slice(&address_word(&descriptor.payee));
    words.extend_from_slice(&address_word(&descriptor.operator));
    words.extend_from_slice(&address_word(&descriptor.token));
    words.extend_from_slice(&descriptor.salt);
    words.extend_from_slice(&address_word(&descriptor.authorized_signer));
    words.extend_from_slice(&descriptor.expiring_nonce_hash);
    words.extend_from_slice(&address_word(escrow));
    words.extend_from_slice(&uint_word(u128::from(chain_id)));
    keccak256(&words)
}

/// The address whose signature a voucher (or close authorization) must carry:
/// `authorized_signer`, or the payer when that is the zero address.
pub fn voucher_signer(descriptor: &ChannelDescriptor) -> [u8; 20] {
    if descriptor.authorized_signer == [0u8; 20] {
        descriptor.payer
    } else {
        descriptor.authorized_signer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::session_vectors::Vectors;

    #[test]
    fn channel_id_matches_the_vector() {
        let v = Vectors::load();
        assert_eq!(
            channel_id(&v.descriptor(), &v.escrow(), v.chain_id()),
            v.fixed::<32>("channelId")
        );
    }

    #[test]
    fn channel_id_binds_every_field_and_their_order() {
        let v = Vectors::load();
        let d = v.descriptor();
        let base = channel_id(&d, &v.escrow(), v.chain_id());
        let mut variants = Vec::new();
        for edit in 0..7 {
            let mut changed = d;
            match edit {
                0 => changed.payer[19] ^= 1,
                1 => changed.payee[19] ^= 1,
                2 => changed.operator[19] ^= 1,
                3 => changed.token[19] ^= 1,
                4 => changed.salt[31] ^= 1,
                5 => changed.authorized_signer[19] ^= 1,
                _ => changed.expiring_nonce_hash[31] ^= 1,
            }
            variants.push(channel_id(&changed, &v.escrow(), v.chain_id()));
        }
        variants.push(channel_id(&d, &[0x11; 20], v.chain_id()));
        variants.push(channel_id(&d, &v.escrow(), v.chain_id() + 1));
        // Swapping payer and payee (different word order) is a different channel.
        let swapped = ChannelDescriptor {
            payer: d.payee,
            payee: d.payer,
            ..d
        };
        variants.push(channel_id(&swapped, &v.escrow(), v.chain_id()));
        for variant in &variants {
            assert_ne!(*variant, base);
        }
        let mut unique = variants.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), variants.len());
    }

    #[test]
    fn the_voucher_signer_defaults_to_the_payer() {
        let v = Vectors::load();
        let d = v.descriptor();
        assert_eq!(d.authorized_signer, [0u8; 20]);
        assert_eq!(voucher_signer(&d), d.payer);
        let delegated = ChannelDescriptor {
            authorized_signer: [0x42; 20],
            ..d
        };
        assert_eq!(voucher_signer(&delegated), [0x42; 20]);
    }
}
