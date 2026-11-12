use ring::hmac;

const KEY_FINGERPRINT_LABEL: &[u8] = b"near-key-fingerprint/v1";

/// First 16 lowercase hex characters of HMAC-SHA256(key, "near-key-fingerprint/v1").
pub fn key_fingerprint(key: &[u8; 32]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&key, KEY_FINGERPRINT_LABEL);
    hex::encode(&tag.as_ref()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_fingerprint_matches_known_hmac_sha256_vectors() {
        // Independently generated with OpenSSL dgst -sha256 -mac HMAC.
        for (key, expected) in [
            ([0; 32], "df02db60e1658041"),
            ([9; 32], "825674e6c3e8ca73"),
            ([7; 32], "820ed242bff6e2d9"),
        ] {
            assert_eq!(key_fingerprint(&key), expected);
        }
    }
}
