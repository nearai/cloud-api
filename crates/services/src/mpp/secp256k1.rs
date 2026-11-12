//! secp256k1 signatures over 32-byte digests, in the 65-byte `r || s || v` form that
//! Ethereum and Tempo use.
//!
//! Only this scheme is accepted. A signature that is not exactly 65 bytes (a TIP-1020
//! P-256 or WebAuthn signature carries a type prefix and a different length) is refused
//! with the closed [`SigError::UnsupportedSignature`], so it fails clearly instead of
//! being misread as secp256k1.
//!
//! Recovery is strict: `v` is one of `0, 1, 27, 28`; `r` and `s` are non-zero scalars
//! below the group order; `s` is in the lower half (a high-`s` twin of a valid signature
//! is malleable and is refused). Signing is deterministic (RFC 6979) and always produces
//! the low-`s` form with `v = 27 + parity`.
//!
//! An address is the last 20 bytes of `keccak256` of the uncompressed public key without
//! its `0x04` prefix.

use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use k256::elliptic_curve::scalar::IsHigh;
use k256::SecretKey;

use super::tempo::keccak256;

/// Length of an `r || s || v` signature.
pub const SIGNATURE_LEN: usize = 65;

/// Why a signature was refused or could not be made. A closed set, carrying no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SigError {
    /// Not 65 bytes: P-256, WebAuthn and any other scheme land here.
    #[error("unsupported signature type")]
    UnsupportedSignature,
    /// `v` is not one of 0, 1, 27, 28.
    #[error("invalid recovery id")]
    InvalidRecoveryId,
    /// `r` or `s` is zero or not below the group order.
    #[error("invalid signature scalar")]
    InvalidScalar,
    /// `s` is in the upper half of the group order.
    #[error("high-s signature")]
    HighS,
    /// No public key recovers from the signature and digest.
    #[error("recovery failed")]
    RecoveryFailed,
}

impl SigError {
    /// Closed code for logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnsupportedSignature => "unsupported_signature",
            Self::InvalidRecoveryId => "sig_recovery_id",
            Self::InvalidScalar => "sig_scalar",
            Self::HighS => "sig_high_s",
            Self::RecoveryFailed => "sig_recovery_failed",
        }
    }
}

/// The Ethereum address of a public key.
fn address_of_verifying_key(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_sec1_point(false);
    // Uncompressed SEC1: 0x04 followed by x and y (64 bytes).
    let xy = point.as_bytes().get(1..).unwrap_or(&[]);
    let hash = keccak256(xy);
    let mut address = [0u8; 20];
    for (slot, byte) in address.iter_mut().zip(hash.iter().skip(12)) {
        *slot = *byte;
    }
    address
}

/// The Ethereum address of a secret key.
pub fn address_of(key: &SecretKey) -> [u8; 20] {
    address_of_verifying_key(SigningKey::from(key).verifying_key())
}

/// Recover the address that signed `digest`.
pub fn recover_address(digest: &[u8; 32], signature: &[u8]) -> Result<[u8; 20], SigError> {
    let signature: &[u8; SIGNATURE_LEN] = signature
        .try_into()
        .map_err(|_| SigError::UnsupportedSignature)?;
    let (rs, v) = signature.split_at(64);
    let parity = match v {
        [0] | [27] => false,
        [1] | [28] => true,
        _ => return Err(SigError::InvalidRecoveryId),
    };
    let signature = Signature::from_slice(rs).map_err(|_| SigError::InvalidScalar)?;
    if bool::from(signature.s().is_high()) {
        return Err(SigError::HighS);
    }
    let key =
        VerifyingKey::recover_from_prehash(digest, &signature, RecoveryId::new(parity, false))
            .map_err(|_| SigError::RecoveryFailed)?;
    Ok(address_of_verifying_key(&key))
}

/// Sign `digest`: deterministic, low-`s`, `v = 27 + parity`.
pub fn sign_digest(key: &SecretKey, digest: &[u8; 32]) -> Result<[u8; SIGNATURE_LEN], SigError> {
    let signing_key = SigningKey::from(key);
    let (signature, recovery_id) = signing_key.sign_prehash_recoverable(digest);
    // Canonicalize to low-s; negating s flips the parity of the recovered point.
    let (signature, y_odd) = if bool::from(signature.s().is_high()) {
        (signature.normalize_s(), !recovery_id.is_y_odd())
    } else {
        (signature, recovery_id.is_y_odd())
    };
    // An x-reduced recovery id (r overflowed the group order) cannot be written in `v`.
    if recovery_id.is_x_reduced() {
        return Err(SigError::RecoveryFailed);
    }
    let mut out = [0u8; SIGNATURE_LEN];
    let (rs, v) = out.split_at_mut(64);
    rs.copy_from_slice(&signature.to_bytes());
    for slot in v.iter_mut() {
        *slot = 27 + u8::from(y_odd);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretKey {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        SecretKey::from_slice(&bytes).unwrap()
    }

    /// The order of secp256k1, big endian.
    const ORDER: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36,
        0x41, 0x41,
    ];

    /// `a - b` for big-endian 32-byte numbers with `a >= b`.
    fn sub(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = i16::from(a[i]) - i16::from(b[i]) - borrow;
            borrow = if d < 0 { 1 } else { 0 };
            if d < 0 {
                d += 256;
            }
            out[i] = d as u8;
        }
        out
    }

    #[test]
    fn a_signature_round_trips_to_the_signer() {
        let digest = keccak256(b"digest");
        for byte in 1..=40u8 {
            let key = key(byte);
            let signature = sign_digest(&key, &digest).unwrap();
            assert!(matches!(signature[64], 27 | 28));
            assert_eq!(recover_address(&digest, &signature), Ok(address_of(&key)));
            assert_eq!(
                sign_digest(&key, &digest).unwrap(),
                signature,
                "deterministic"
            );
        }
    }

    #[test]
    fn signing_always_yields_low_s_even_when_the_raw_signature_is_high() {
        // Across many digests about half of raw RFC 6979 signatures have a high s.
        let key = key(7);
        for i in 0..64u32 {
            let digest = keccak256(&i.to_be_bytes());
            let signature = sign_digest(&key, &digest).unwrap();
            let parsed = Signature::from_slice(&signature[..64]).unwrap();
            assert!(!bool::from(parsed.s().is_high()));
            assert_eq!(recover_address(&digest, &signature), Ok(address_of(&key)));
        }
    }

    #[test]
    fn v_accepts_zero_one_27_and_28_only() {
        let digest = keccak256(b"v");
        let key = key(3);
        let signature = sign_digest(&key, &digest).unwrap();
        let signer = address_of(&key);
        let parity = signature[64] - 27;
        for (v, ok) in [
            (parity, true),
            (27 + parity, true),
            (1 - parity, false),
            (28 - parity, false),
        ] {
            let mut candidate = signature;
            candidate[64] = v;
            let recovered = recover_address(&digest, &candidate);
            // The wrong parity recovers some other key (or fails), never the signer.
            assert_eq!(recovered == Ok(signer), ok, "v = {v}");
        }
        for v in [2u8, 3, 26, 29, 35, 36, 0xff] {
            let mut candidate = signature;
            candidate[64] = v;
            assert_eq!(
                recover_address(&digest, &candidate),
                Err(SigError::InvalidRecoveryId),
                "v = {v}"
            );
        }
    }

    #[test]
    fn only_65_byte_signatures_are_supported() {
        let digest = keccak256(b"len");
        let signature = sign_digest(&key(1), &digest).unwrap();
        for len in [0usize, 1, 64, 66, 129, 200] {
            let bytes = vec![signature[0]; len];
            assert_eq!(
                recover_address(&digest, &bytes),
                Err(SigError::UnsupportedSignature),
                "len {len}"
            );
        }
        // A type-prefixed P-256 style signature (0x01 || r || s || pubkey ...) is not 65 bytes.
        let mut p256 = vec![0x01];
        p256.extend([0x11; 128]);
        assert_eq!(
            recover_address(&digest, &p256),
            Err(SigError::UnsupportedSignature)
        );
    }

    #[test]
    fn high_s_zero_and_out_of_range_scalars_are_refused() {
        let digest = keccak256(b"scalars");
        let key = key(5);
        let signature = sign_digest(&key, &digest).unwrap();

        // The high-s twin of a valid signature: s' = n - s, with the parity flipped.
        let mut s = [0u8; 32];
        s.copy_from_slice(&signature[32..64]);
        let high = sub(&ORDER, &s);
        let mut twin = signature;
        twin[32..64].copy_from_slice(&high);
        twin[64] = if signature[64] == 27 { 28 } else { 27 };
        assert_eq!(recover_address(&digest, &twin), Err(SigError::HighS));

        let mut zero_r = signature;
        zero_r[..32].fill(0);
        assert_eq!(
            recover_address(&digest, &zero_r),
            Err(SigError::InvalidScalar)
        );
        let mut zero_s = signature;
        zero_s[32..64].fill(0);
        assert_eq!(
            recover_address(&digest, &zero_s),
            Err(SigError::InvalidScalar)
        );
        let mut big_r = signature;
        big_r[..32].copy_from_slice(&ORDER);
        assert_eq!(
            recover_address(&digest, &big_r),
            Err(SigError::InvalidScalar)
        );
    }

    #[test]
    fn a_tampered_signature_or_digest_does_not_recover_the_signer() {
        let digest = keccak256(b"tamper");
        let key = key(9);
        let signature = sign_digest(&key, &digest).unwrap();
        let signer = address_of(&key);
        let other = keccak256(b"other");
        assert_ne!(recover_address(&other, &signature), Ok(signer));
        let mut flipped = signature;
        flipped[10] ^= 1;
        assert_ne!(recover_address(&digest, &flipped), Ok(signer));
    }

    #[test]
    fn address_of_key_one_is_the_well_known_address() {
        // Secret key 1 -> 0x7e5f4552091a69125d5dfcb7b8c2659029395bdf.
        let address = address_of(&key(1));
        assert_eq!(
            hex::encode(address),
            "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }
    #[test]
    fn the_vector_signatures_recover_to_the_payer_and_the_keys_to_their_addresses() {
        use crate::mpp::session_vectors::Vectors;
        let v = Vectors::load();
        let payer = v.fixed::<20>("payer");
        assert_eq!(address_of(&v.secret_key("payerKey")), payer);
        assert_eq!(
            address_of(&v.secret_key("payeeKey")),
            v.fixed::<20>("payee")
        );
        for (digest, signature) in [
            ("voucher.digest", "voucher.signature"),
            ("closeAuthorization.digest", "closeAuthorization.signature"),
        ] {
            assert_eq!(
                recover_address(&v.fixed::<32>(digest), &v.bytes(signature)),
                Ok(payer),
                "{signature}"
            );
        }
    }

    #[test]
    fn signing_the_vector_digests_matches_the_vector_signatures() {
        use crate::mpp::session_vectors::Vectors;
        let v = Vectors::load();
        let payer_key = v.secret_key("payerKey");
        for (digest, signature) in [
            ("voucher.digest", "voucher.signature"),
            ("closeAuthorization.digest", "closeAuthorization.signature"),
        ] {
            let ours = sign_digest(&payer_key, &v.fixed::<32>(digest)).unwrap();
            assert_eq!(
                recover_address(&v.fixed::<32>(digest), &ours),
                Ok(v.fixed::<20>("payer"))
            );
            // RFC 6979 with low-s is what viem produces, so the bytes are identical.
            assert_eq!(ours.to_vec(), v.bytes(signature), "{signature}");
        }
    }
}
