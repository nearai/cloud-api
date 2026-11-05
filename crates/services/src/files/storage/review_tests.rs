use super::*;
use crate::files::encryption::{encrypt_with_key, ObjectKeyRing};
use std::sync::Arc;

fn storage() -> MockStorage {
    MockStorage::new_with_ring(Arc::new(
        ObjectKeyRing::new("current".into(), [1; 32], vec![("old".into(), [2; 32])]).unwrap(),
    ))
}

#[tokio::test]
async fn metadata_includes_the_encrypted_content_length() {
    // Given one uploaded object with an authenticated-encryption envelope.
    let storage = storage();
    storage
        .upload("file", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // When metadata is read.
    let metadata = storage.object_metadata("file").await.unwrap();
    // Then it reports the exact bytes that would be downloaded.
    assert_eq!(
        metadata,
        ObjectMetadata {
            key_id: Some("current".into()),
            content_length: 35
        }
    );
}

#[tokio::test]
async fn oversized_objects_report_whether_metadata_is_current_without_rewriting() {
    // Given oversized current and old objects.
    let storage = storage();
    for (key, id, bytes) in [("current", "current", [1; 32]), ("old", "old", [2; 32])] {
        storage
            .seed_raw(
                key,
                encrypt_with_key(b"content", &bytes).unwrap(),
                Some(id.into()),
                "text/plain",
            )
            .unwrap();
    }
    // When rewrap and verify use a limit below their encrypted size.
    for (key, key_state) in [
        ("current", ObjectKeyState::Current),
        ("old", ObjectKeyState::StaleOrAbsent),
    ] {
        let rewrap = storage.rewrap_object(key, 34).await;
        let verify = storage.verify_object(key, 34).await;
        // Then both report the limit and preserve the original metadata.
        assert!(
            matches!(rewrap, Err(FileServiceError::ObjectTooLarge { key_state: value }) if value == key_state)
        );
        assert!(
            matches!(verify, Err(FileServiceError::ObjectTooLarge { key_state: value }) if value == key_state)
        );
        assert_eq!(
            storage.object_key_id(key).await.unwrap().as_deref(),
            Some(key)
        );
    }
}

#[tokio::test]
async fn verify_retains_unknown_id_when_object_grows_after_preflight() {
    // Given a small known object replaced by an oversized object with an unknown id.
    let storage = storage();
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("old".into()),
            "text/plain",
        )
        .unwrap();
    let preflight = storage.object_metadata("file").await.unwrap();
    assert_eq!(preflight.content_length, 35);
    assert_eq!(preflight.key_id.as_deref(), Some("old"));
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content+", &[2; 32]).unwrap(),
            Some("unknown".into()),
            "text/plain",
        )
        .unwrap();
    // When verification processes the replacement with the original size limit.
    let result = storage.verify_object("file", 35).await;
    // Then both authoritative classifications survive the size rejection.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::UnknownKeyId
        })
    ));
}

#[tokio::test]
async fn execute_retains_unknown_id_when_object_grows_after_preflight() {
    // Given a small known object replaced by an oversized object with an unknown id.
    let storage = storage();
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("old".into()),
            "text/plain",
        )
        .unwrap();
    let preflight = storage.object_metadata("file").await.unwrap();
    assert_eq!(preflight.content_length, 35);
    assert_eq!(preflight.key_id.as_deref(), Some("old"));
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content+", &[2; 32]).unwrap(),
            Some("unknown".into()),
            "text/plain",
        )
        .unwrap();
    // When execution processes the replacement with the original size limit.
    let result = storage.rewrap_object("file", 35).await;
    // Then both authoritative classifications survive without rewriting the object.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::UnknownKeyId
        })
    ));
}

#[tokio::test]
async fn unknown_metadata_is_undecryptable_during_verify_even_with_write_ciphertext() {
    // Given readable ciphertext labeled with an unknown key id.
    let storage = storage();
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content", &[1; 32]).unwrap(),
            Some("unknown".into()),
            "text/plain",
        )
        .unwrap();
    // When verification examines the object.
    let result = storage.verify_object("file", 35).await.unwrap();
    // Then it never silently trials the ring.
    assert_eq!(
        result,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: true,
        }
    );
}

#[tokio::test]
async fn execute_can_recover_unknown_metadata_using_the_ring() {
    // Given previous-key ciphertext labeled with an unknown key id.
    let storage = storage();
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("unknown".into()),
            "text/plain",
        )
        .unwrap();
    // When execute rewraps the object at exactly the byte limit.
    let outcome = storage.rewrap_object("file", 35).await.unwrap();
    // Then the unknown label is reported and the new ciphertext is readable.
    assert_eq!(outcome, RewrapOutcome::RewrappedUnknownKeyId);
    assert_eq!(storage.download("file").await.unwrap(), b"content");
}

#[tokio::test]
async fn verify_reports_unknown_metadata_replaced_after_preflight() {
    // Given a known id observed during preflight, then replaced with an unknown id.
    let storage = storage();
    let encrypted = encrypt_with_key(b"content", &[2; 32]).unwrap();
    storage
        .seed_raw("file", encrypted.clone(), Some("old".into()), "text/plain")
        .unwrap();
    assert_eq!(
        storage
            .object_metadata("file")
            .await
            .unwrap()
            .key_id
            .as_deref(),
        Some("old")
    );
    storage
        .seed_raw("file", encrypted, Some("unknown".into()), "text/plain")
        .unwrap();
    // When verification reads the authoritative object.
    let result = storage.verify_object("file", 35).await.unwrap();
    // Then the reason stays distinct from a known-key authentication failure.
    assert_eq!(
        result,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: true
        }
    );
}

#[tokio::test]
async fn execute_reports_unknown_metadata_replaced_after_preflight() {
    // Given a known id observed during preflight, then replaced with an unknown id.
    let storage = storage();
    let encrypted = encrypt_with_key(b"content", &[2; 32]).unwrap();
    storage
        .seed_raw("file", encrypted.clone(), Some("old".into()), "text/plain")
        .unwrap();
    assert_eq!(
        storage
            .object_metadata("file")
            .await
            .unwrap()
            .key_id
            .as_deref(),
        Some("old")
    );
    storage
        .seed_raw("file", encrypted, Some("unknown".into()), "text/plain")
        .unwrap();
    // When execution recovers the actual ciphertext.
    let outcome = storage.rewrap_object("file", 35).await.unwrap();
    // Then the caller can count the newly discovered unknown id.
    assert_eq!(outcome, RewrapOutcome::RewrappedUnknownKeyId);
}

#[tokio::test]
async fn execute_distinguishes_unknown_metadata_when_no_ring_key_decrypts() {
    // Given unknown metadata and ciphertext from a key outside the ring.
    let storage = storage();
    storage
        .seed_raw(
            "file",
            encrypt_with_key(b"content", &[3; 32]).unwrap(),
            Some("unknown".into()),
            "text/plain",
        )
        .unwrap();
    // When execution tries to recover the object.
    let outcome = storage.rewrap_object("file", 35).await.unwrap();
    // Then the unknown-id class is retained even though recovery failed.
    assert_eq!(outcome, RewrapOutcome::UndecryptableUnknownKeyId);
}

#[tokio::test]
async fn readable_legacy_and_previous_objects_are_not_undecryptable() {
    let storage = storage();
    for hint in [None, Some("old")] {
        storage
            .seed_raw(
                "file",
                encrypt_with_key(b"content", &[2; 32]).unwrap(),
                hint.map(str::to_owned),
                "text/plain",
            )
            .unwrap();
        let result = storage.verify_object("file", 35).await.unwrap();
        assert!(
            result.decrypts_under_previous_key,
            "previous-key ciphertext remains readable"
        );
        assert!(!result.decrypts_under_write_key);
    }
}

#[tokio::test]
async fn execute_trusts_current_metadata_but_verify_authenticates_ciphertext() {
    let storage = storage();
    storage
        .seed_raw("file", vec![0; 35], Some("current".into()), "text/plain")
        .unwrap();
    assert_eq!(
        storage.rewrap_object("file", 35).await.unwrap(),
        RewrapOutcome::AlreadyCurrent
    );
    let verification = storage.verify_object("file", 35).await.unwrap();
    assert!(verification.is_current);
    assert!(!verification.decrypts_under_write_key);
}
