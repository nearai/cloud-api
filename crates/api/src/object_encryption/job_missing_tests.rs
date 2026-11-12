use super::{progress::Progress, types::Mode, worker, ObjectEncryptionState};
use database::DbPool;
use services::files::{encryption::ObjectKeyRing, storage::StorageTrait, FileServiceError};
use std::sync::Arc;

#[path = "../../tests/common/object_encryption_missing.rs"]
mod storage_fixture;

async fn process_fixture(mode: Mode, fixture_mode: &str) -> Progress {
    // Given the exact E2E storage fixture, using one-object job pages.
    let ring = Arc::new(
        ObjectKeyRing::new("s3-v2".into(), [9; 32], vec![("s3-v1".into(), [7; 32])]).unwrap(),
    );
    let (storage, inner) = storage_fixture::storage(fixture_mode, false, ring.clone()).await;
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), storage, ring);
    let (keys, next) = state.storage.list_keys(None, 1).await.unwrap();
    assert_eq!(keys, ["a-deleted"]);
    assert_eq!(next.as_deref(), Some("a-deleted"));
    assert!(!inner.exists("a-deleted").await.unwrap());
    assert!(matches!(
        state.storage.object_key_id("a-deleted").await,
        Err(FileServiceError::NotFound)
    ));

    // When the worker processes the deleted key and restores progress for the next page.
    let mut progress = Progress::default();
    worker::process_key(&state, mode, &keys[0], &mut progress)
        .await
        .unwrap();
    assert_eq!(progress.missing, 1);
    assert_eq!(progress.missing_keys, ["a-deleted"]);
    assert_eq!(progress.processed, 1);
    assert!(progress.by_key_id.is_empty());
    let mut progress: Progress =
        serde_json::from_value(serde_json::to_value(progress).unwrap()).unwrap();
    let (keys, end) = state.storage.list_keys(next, 1).await.unwrap();
    assert_eq!(keys, ["b-retained"]);
    assert!(end.is_none());
    worker::process_key(&state, mode, &keys[0], &mut progress)
        .await
        .unwrap();

    // Then deletion is neither a legacy metadata entry nor a verification failure.
    assert_eq!(progress.missing, 1);
    assert_eq!(progress.missing_keys, ["a-deleted"]);
    assert_eq!(progress.processed, 2);
    assert_eq!(progress.total, 2);
    assert_eq!(progress.undecryptable, 0);
    assert!(progress.undecryptable_keys.is_empty());
    progress
}

#[tokio::test]
async fn dry_run_counts_deleted_objects_without_inventorying_them() {
    let progress = process_fixture(Mode::DryRun, "dry_run").await;
    assert_eq!(progress.by_key_id, [("s3-v2".into(), 1)].into());
}

#[tokio::test]
async fn execute_skips_deleted_objects_and_rewraps_the_retained_object() {
    let progress = process_fixture(Mode::Execute, "execute").await;
    assert_eq!(progress.rewrapped, 1);
    assert_eq!(progress.already_current, 0);
}

#[tokio::test]
async fn verify_skips_deleted_objects_without_failing_the_survivor() {
    let progress = process_fixture(Mode::Verify, "verify").await;
    assert_eq!(progress.verified, 1);
    assert_eq!(progress.stale, 0);
}

#[tokio::test]
async fn dry_run_counts_retained_legacy_metadata_separately_from_deleted_objects() {
    let progress = process_fixture(Mode::DryRun, "execute").await;
    assert_eq!(progress.by_key_id, [("none".into(), 1)].into());
}
