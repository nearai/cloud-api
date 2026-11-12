use super::*;

#[test]
fn verification_preserves_strict_known_ids_and_legacy_trials() {
    let ring = ObjectKeyRing::new(
        "current".into(),
        [1; 32],
        vec![("old".into(), [2; 32]), ("older".into(), [3; 32])],
    )
    .unwrap();
    for (hint, key, write, previous, unknown) in [
        (None, [1; 32], true, false, false),
        (None, [2; 32], false, true, false),
        (None, [3; 32], false, true, false),
        (Some("old"), [2; 32], false, true, false),
        (Some("current"), [2; 32], false, false, false),
        (Some("old"), [1; 32], false, false, false),
        (Some("old"), [3; 32], false, false, false),
        (Some("unknown"), [1; 32], false, false, true),
        (None, [9; 32], false, false, false),
    ] {
        let encrypted = encryption::encrypt_with_key(b"content", &key).unwrap();
        assert_eq!(
            ObjectVerification::authenticate(&encrypted, &ring, hint),
            ObjectVerification {
                is_current: hint == Some("current"),
                decrypts_under_write_key: write,
                decrypts_under_previous_key: previous,
                unknown_key_id: unknown,
            },
            "hint={hint:?}"
        );
    }
}
