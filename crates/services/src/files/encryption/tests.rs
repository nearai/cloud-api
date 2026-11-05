use super::*;

fn ring() -> ObjectKeyRing {
    ObjectKeyRing::new(
        "s3-v3".into(),
        [3; 32],
        vec![("s3-v2".into(), [2; 32]), ("s3-v1".into(), [1; 32])],
    )
    .unwrap()
}

#[test]
fn ordered_keys_read_legacy_objects_and_return_the_matching_id() {
    let ring = ring();
    assert_eq!(ring.ids(), vec!["s3-v3", "s3-v2", "s3-v1"]);
    for (id, key) in [("s3-v3", [3; 32]), ("s3-v2", [2; 32]), ("s3-v1", [1; 32])] {
        let data = encrypt_with_key(b"file content", &key).unwrap();
        assert_eq!(
            decrypt_with_ring(&data, &ring, None).unwrap(),
            (b"file content".to_vec(), id.into())
        );
    }
}

#[test]
fn an_existing_hint_selects_only_that_key() {
    let data = encrypt_with_key(b"file content", &[1; 32]).unwrap();
    let ring = ring();
    assert!(decrypt_with_ring(&data, &ring, Some("s3-v3")).is_err());
    assert_eq!(
        decrypt_with_ring(&data, &ring, Some("s3-v1")).unwrap().1,
        "s3-v1"
    );
    assert_eq!(
        decrypt_with_ring(&data, &ring, Some("retired-id"))
            .unwrap()
            .1,
        "s3-v1"
    );
}

#[test]
fn identical_material_under_distinct_ids_is_rejected() {
    for previous in [
        vec![("old".into(), [1; 32])],
        vec![("old".into(), [2; 32]), ("older".into(), [2; 32])],
    ] {
        assert!(ObjectKeyRing::new("current".into(), [1; 32], previous).is_err());
    }
}

#[test]
fn ring_zeroizes_its_keys_and_declares_drop_cleanup() {
    fn assert_zeroizes_on_drop<T: zeroize::ZeroizeOnDrop>() {}
    assert_zeroizes_on_drop::<ObjectKeyRing>();
    let mut ring = ring();
    zeroize::Zeroize::zeroize(&mut ring);
    assert_eq!(ring.write_key(), &[0; 32]);
    assert_eq!(ring.ids(), vec![""]);
}

#[test]
fn ciphertext_under_a_missing_key_is_rejected() {
    let data = encrypt_with_key(b"file content", &[9; 32]).unwrap();
    assert!(decrypt_with_ring(&data, &ring(), None).is_err());
    assert!(decrypt_with_ring(b"short", &ring(), None).is_err());
}

#[test]
fn hex_adapters_and_parsed_keys_share_the_envelope() {
    let key = [7; 32];
    let hex = hex::encode(key);
    let parsed = encrypt_with_key(b"content", &key).unwrap();
    assert_eq!(parsed.len(), 12 + b"content".len() + 16);
    assert_eq!(decrypt(&parsed, &hex).unwrap(), b"content");
    let adapted = encrypt(b"content", &hex).unwrap();
    assert_eq!(decrypt_with_key(&adapted, &key).unwrap(), b"content");
    let legacy = ObjectKeyRing::from_hex(&hex).unwrap();
    assert_eq!(legacy.write_id(), "s3-legacy");
    assert_eq!(legacy.write_key(), &key);
}

#[test]
fn key_ids_are_validated_and_debug_contains_only_ids() {
    for id in ["", "bad id", "name/path"] {
        assert!(ObjectKeyRing::new(id.into(), [1; 32], vec![]).is_err());
    }
    assert!(ObjectKeyRing::new("a".repeat(65), [1; 32], vec![]).is_err());
    assert!(ObjectKeyRing::new("same".into(), [1; 32], vec![("same".into(), [2; 32])]).is_err());
    assert_eq!(
        format!("{:?}", ring()),
        "ObjectKeyRing { ids: [\"s3-v3\", \"s3-v2\", \"s3-v1\"] }"
    );
}

#[test]
fn fingerprints_match_the_database_convention() {
    assert_eq!(
        ring().fingerprints(),
        vec![
            ("s3-v3".into(), "648aa5c579fb30f3".into()),
            ("s3-v2".into(), "75877bb41d393b5f".into()),
            ("s3-v1".into(), "72cd6e8422c407fb".into()),
        ]
    );
}

#[test]
fn invalid_hex_and_lengths_fail_without_key_material() {
    for raw in ["private-key-text".into(), "11".repeat(31)] {
        let error = ObjectKeyRing::from_hex(&raw).unwrap_err().to_string();
        assert!(!error.contains(&raw));
    }
}
