use super::{
    object_processing::STORAGE_RETRY_ATTEMPTS,
    progress::Progress,
    types::Mode,
    worker,
    worker_test_storage::{fault, mock, ring, Failure},
    ObjectEncryptionState,
};
use database::DbPool;
use services::files::encryption;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
#[tokio::test]
async fn conditional_race_is_changed_instead_of_a_job_failure() {
    let (state, attempts) = fault(Failure::Changed).await;
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!(
        (progress.changed, progress.errors, progress.processed),
        (1, 0, 1)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transient_errors_retry_then_process_the_object() {
    let (state, attempts) = fault(Failure::Transient(3)).await;
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!((progress.errors, progress.already_current), (0, 1));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        STORAGE_RETRY_ATTEMPTS as usize + 1
    );
}

#[tokio::test]
async fn exhausted_retries_record_a_sanitized_error() {
    let (state, attempts) = fault(Failure::Transient(20)).await;
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        STORAGE_RETRY_ATTEMPTS as usize + 1
    );
    assert_eq!(progress.errors, 1);
    assert_eq!(progress.error_samples[0].class, "transient_storage_error");
    assert!(!serde_json::to_string(&progress)
        .unwrap()
        .contains("private details"));
}

#[tokio::test]
async fn persistent_errors_continue_without_retries() {
    let (state, attempts) = fault(Failure::Persistent).await;
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!((progress.errors, progress.processed), (1, 1));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timeouts_retry_three_times_and_continue() {
    let (mut state, attempts) = fault(Failure::Timeout).await;
    state.storage_timeout = Duration::from_millis(1);
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        STORAGE_RETRY_ATTEMPTS as usize + 1
    );
    assert_eq!(progress.error_samples[0].class, "timeout");
}

#[tokio::test]
async fn oversized_objects_never_reach_rewrap() {
    let (state, attempts) = fault(Failure::Persistent).await;
    let state = state.with_job_options(1, true);
    let mut progress = Progress::default();
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    assert_eq!((progress.oversized, progress.errors), (1, 0));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn oversized_verification_never_passes_without_authentication() {
    for (key_id, stale) in [("current", 0), ("previous", 1)] {
        let storage = mock().await;
        storage
            .seed_raw("object", vec![0; 50], Some(key_id.into()), "text/plain")
            .unwrap();
        let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(storage), ring())
            .with_job_options(1, true);
        let mut progress = Progress {
            complete: true,
            ..Progress::default()
        };
        worker::process_key(&state, Mode::Verify, "object", &mut progress)
            .await
            .unwrap();
        assert_eq!((progress.oversized, progress.stale), (1, stale));
        assert!(
            !progress.verification_passes(),
            "an unauthenticated oversized object cannot pass verification"
        );
    }
}

#[tokio::test]
async fn unknown_metadata_is_undecryptable_during_scan_and_verify_but_recoverable_during_execute() {
    for mode in [Mode::DryRun, Mode::Verify, Mode::Execute] {
        let storage = mock().await;
        storage
            .seed_raw(
                "object",
                encryption::encrypt_with_key(b"payload", &[7; 32]).unwrap(),
                Some("unknown".into()),
                "text/plain",
            )
            .unwrap();
        let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(storage), ring());
        let mut progress = Progress::default();
        worker::process_key(&state, mode, "object", &mut progress)
            .await
            .unwrap();
        assert_eq!(progress.unknown_key_ids, 1);
        match mode {
            Mode::Execute => assert_eq!((progress.rewrapped, progress.undecryptable), (1, 0)),
            Mode::DryRun | Mode::Verify => {
                assert_eq!(progress.undecryptable, 1);
                assert_eq!(progress.undecryptable_classes["unknown_key_id"], 1);
            }
        }
    }
}

#[tokio::test]
async fn disabled_execute_gate_prevents_any_object_access() {
    let (state, attempts) = fault(Failure::Persistent).await;
    let state = state.with_job_options(256, false);
    let mut progress = Progress::default();
    assert!(
        worker::process_key(&state, Mode::Execute, "object", &mut progress)
            .await
            .is_err()
    );
    assert_eq!(progress.processed, 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn metadata_changed_to_unknown_before_verify_retains_its_error_class() {
    // Given known metadata at the worker HEAD and unknown metadata at verification.
    let (state, _) = fault(Failure::MetadataRace).await;
    let mut progress = Progress::default();
    // When verification reads the authoritative object metadata.
    worker::process_key(&state, Mode::Verify, "object", &mut progress)
        .await
        .unwrap();
    // Then the race retains unknown-key classification without decrypting.
    assert_eq!(progress.unknown_key_ids, 1);
    assert_eq!(
        progress.undecryptable_classes.get("unknown_key_id"),
        Some(&1)
    );
    assert_eq!((progress.stale, progress.undecryptable), (1, 1));
}

#[tokio::test]
async fn metadata_changed_to_unknown_before_rewrap_is_counted_for_the_job_warning() {
    // Given known metadata at the worker HEAD and unknown metadata at rewrite.
    let (state, _) = fault(Failure::MetadataRace).await;
    let mut progress = Progress::default();
    // When execute recovers the object using a configured read key.
    worker::process_key(&state, Mode::Execute, "object", &mut progress)
        .await
        .unwrap();
    // Then one recovered object is also counted for the job-level warning.
    assert_eq!(progress.unknown_key_ids, 1);
    assert_eq!((progress.rewrapped, progress.undecryptable), (1, 0));
}

#[tokio::test]
async fn unknown_oversized_metadata_race_keeps_both_outcomes() {
    // Given a small known object at preflight and a large unknown object at the storage operation.
    for mode in [Mode::Verify, Mode::Execute] {
        let (state, _) = fault(Failure::MetadataOversizedRace).await;
        let state = state.with_job_options(128, true);
        let mut progress = Progress::default();
        // When the authoritative size check rejects the changed object before download.
        worker::process_key(&state, mode, "object", &mut progress)
            .await
            .unwrap();
        // Then both identities are reported without claiming a rewrite or verification.
        assert_eq!(
            (
                progress.oversized,
                progress.unknown_key_ids,
                progress.errors
            ),
            (1, 1, 0)
        );
        assert_eq!((progress.rewrapped, progress.verified), (0, 0));
        match mode {
            Mode::Verify => {
                assert_eq!((progress.stale, progress.undecryptable), (1, 1));
                assert_eq!(
                    progress.undecryptable_classes.get("unknown_key_id"),
                    Some(&1)
                );
            }
            Mode::Execute => assert_eq!(progress.undecryptable, 0),
            Mode::DryRun => unreachable!(),
        }
    }
}

#[tokio::test]
async fn readable_previous_key_objects_are_stale_not_undecryptable() {
    for hint in [None, Some("previous".into())] {
        let storage = mock().await;
        storage
            .seed_raw(
                "object",
                encryption::encrypt_with_key(b"payload", &[7; 32]).unwrap(),
                hint,
                "text/plain",
            )
            .unwrap();
        let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(storage), ring());
        let mut progress = Progress::default();
        worker::process_key(&state, Mode::Verify, "object", &mut progress)
            .await
            .unwrap();
        assert_eq!(
            (progress.stale, progress.undecryptable, progress.verified),
            (1, 0, 0)
        );
    }
}
