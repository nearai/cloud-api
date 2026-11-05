use super::*;
use crate::files::encryption::{encrypt_with_key, ObjectKeyRing};
use std::sync::Arc;

fn ring() -> Arc<ObjectKeyRing> {
    Arc::new(
        ObjectKeyRing::new(
            "current".into(),
            [1; 32],
            vec![("previous".into(), [2; 32])],
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn upload_records_the_write_key_id() {
    // Given a store with a current and previous key.
    let storage = MockStorage::new_with_ring(ring());
    // When a new object is uploaded.
    storage
        .upload("workspace/file", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // Then the object metadata names the write key.
    assert_eq!(
        storage
            .object_key_id("workspace/file")
            .await
            .unwrap()
            .as_deref(),
        Some("current")
    );
}

#[tokio::test]
async fn legacy_ciphertext_downloads_without_metadata() {
    // Given an object encrypted with the previous key and no metadata.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .seed_raw(
            "legacy",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    // When it is downloaded through the ring.
    let plaintext = storage.download("legacy").await.unwrap();
    // Then the original content is returned.
    assert_eq!(plaintext, b"content");
}

#[tokio::test]
async fn known_metadata_hint_limits_decryption_to_that_key() {
    // Given previous ciphertext labeled with the current key.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .seed_raw(
            "mismatch",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("current".into()),
            "text/plain",
        )
        .unwrap();
    // When it is downloaded.
    let result = storage.download("mismatch").await;
    // Then the metadata-selected key must authenticate it.
    assert!(matches!(result, Err(FileServiceError::EncryptionError(_))));
}

#[tokio::test]
async fn rewrap_preserves_content_for_a_write_key_only_reader() {
    // Given previous ciphertext with a previous-key hint.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .seed_raw(
            "legacy",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("previous".into()),
            "text/plain",
        )
        .unwrap();
    // When the object is rewrapped.
    let outcome = storage.rewrap_object("legacy", u64::MAX).await.unwrap();
    // Then it carries current metadata and remains readable without the previous key.
    assert_eq!(
        outcome,
        RewrapOutcome::Rewrapped {
            from: "previous".into()
        }
    );
    assert_eq!(
        storage.object_key_id("legacy").await.unwrap().as_deref(),
        Some("current")
    );
    let reader = storage.with_key_ring(Arc::new(
        ObjectKeyRing::new("current".into(), [1; 32], vec![]).unwrap(),
    ));
    assert_eq!(reader.download("legacy").await.unwrap(), b"content");
}

#[tokio::test]
async fn rewrap_skips_current_metadata() {
    // Given an object with current metadata.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .upload("current", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // When it is rewrapped again.
    let outcome = storage.rewrap_object("current", u64::MAX).await.unwrap();
    // Then it is already current.
    assert_eq!(outcome, RewrapOutcome::AlreadyCurrent);
}

#[tokio::test]
async fn rewrap_reports_unknown_ciphertext_without_changing_it() {
    // Given ciphertext from a key outside the ring.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .seed_raw(
            "unknown",
            encrypt_with_key(b"content", &[3; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    // When it is rewrapped.
    let outcome = storage.rewrap_object("unknown", u64::MAX).await.unwrap();
    // Then decryption is reported and the metadata stays absent.
    assert_eq!(outcome, RewrapOutcome::Undecryptable);
    assert_eq!(storage.object_key_id("unknown").await.unwrap(), None);
}

#[tokio::test]
async fn verification_checks_metadata_and_write_key_independently() {
    // Given stale metadata with write-key ciphertext and current metadata with previous-key ciphertext.
    let storage = MockStorage::new_with_ring(ring());
    storage
        .seed_raw(
            "stale",
            encrypt_with_key(b"content", &[1; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    storage
        .seed_raw(
            "wrong",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            Some("current".into()),
            "text/plain",
        )
        .unwrap();
    storage
        .seed_raw(
            "both",
            encrypt_with_key(b"content", &[2; 32]).unwrap(),
            None,
            "text/plain",
        )
        .unwrap();
    // When each object is verified.
    let stale = storage.verify_object("stale", u64::MAX).await.unwrap();
    let wrong = storage.verify_object("wrong", u64::MAX).await.unwrap();
    let both = storage.verify_object("both", u64::MAX).await.unwrap();
    // Then metadata and authentication retain separate results.
    assert_eq!(
        stale,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: true,
            decrypts_under_previous_key: false,
            unknown_key_id: false,
        }
    );
    assert_eq!(
        wrong,
        ObjectVerification {
            is_current: true,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: false,
            unknown_key_id: false,
        }
    );
    assert_eq!(
        both,
        ObjectVerification {
            is_current: false,
            decrypts_under_write_key: false,
            decrypts_under_previous_key: true,
            unknown_key_id: false,
        }
    );
}

#[tokio::test]
async fn list_keys_pages_in_lexical_order_and_resumes_after_deletion() {
    // Given three keys inserted out of order.
    let storage = MockStorage::new_with_ring(ring());
    for key in ["c", "a", "b"] {
        storage.upload(key, vec![], "text/plain").await.unwrap();
    }
    // When the first page's last key is removed before continuing.
    let (first, next) = storage.list_keys(None, 2).await.unwrap();
    storage.delete("b").await.unwrap();
    let (second, end) = storage.list_keys(next, 2).await.unwrap();
    // Then the remaining page is neither skipped nor repeated.
    assert_eq!(first, ["a", "b"]);
    assert_eq!(second, ["c"]);
    assert_eq!(end, None);
}

#[tokio::test]
async fn list_keys_rejects_invalid_page_size() {
    // Given an empty store.
    let storage = MockStorage::new_with_ring(ring());
    // When a zero-sized page is requested.
    let result = storage.list_keys(None, 0).await;
    // Then the request fails instead of returning a non-advancing cursor.
    assert!(matches!(result, Err(FileServiceError::StorageError(_))));
}

#[tokio::test]
async fn missing_object_metadata_returns_not_found() {
    // Given an empty store.
    let storage = MockStorage::new_with_ring(ring());
    // When metadata is requested for an absent key.
    let result = storage.object_key_id("absent").await;
    // Then absence remains distinct from missing metadata.
    assert!(matches!(result, Err(FileServiceError::NotFound)));
}

#[tokio::test]
async fn legacy_constructor_preserves_file_round_trip() {
    // Given the original single-key constructor.
    let storage = MockStorage::new(hex::encode([4; 32])).unwrap();
    storage
        .upload("legacy", b"content".to_vec(), "text/plain")
        .await
        .unwrap();
    // When its object is downloaded.
    let plaintext = storage.download("legacy").await.unwrap();
    // Then the existing content contract is preserved.
    assert_eq!(plaintext, b"content");
}

#[test]
fn legacy_constructor_rejects_malformed_key() {
    // Given a key that is not hexadecimal.
    let key = "invalid".to_owned();
    // When the legacy constructor parses it.
    let result = MockStorage::new(key);
    // Then initialization fails.
    assert!(result.is_err());
}
