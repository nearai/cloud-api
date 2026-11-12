use super::{
    lease::Job,
    progress::{Cursor, Progress},
    types::ObjectCreateJobRequest,
    worker::{self, JobControl},
    worker_test_storage::{mock, ring},
    ObjectEncryptionState,
};
use database::DbPool;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct WarningControl {
    fail: AtomicBool,
    persisted_markers: Mutex<Vec<bool>>,
}

impl JobControl for WarningControl {
    async fn check(&self) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn persist(&self, job: &mut Job, _: &str) -> anyhow::Result<()> {
        self.persisted_markers
            .lock()
            .unwrap()
            .push(job.progress.unknown_key_ids_warned);
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "checkpoint unavailable");
        Ok(())
    }
}

#[tokio::test]
async fn warning_marker_rolls_back_when_the_terminal_checkpoint_fails() {
    // Given an execute job with unknown metadata and an unavailable terminal checkpoint.
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(mock().await), ring());
    let mut job = Job {
        id: uuid::Uuid::new_v4(),
        request: serde_json::from_value::<ObjectCreateJobRequest>(
            serde_json::json!({"mode":"execute"}),
        )
        .unwrap()
        .validated()
        .unwrap(),
        cursor: Cursor {
            pending_keys: vec!["object".into()],
            ..Cursor::default()
        },
        progress: Progress {
            unknown_key_ids: 2,
            ..Progress::default()
        },
    };
    let control = WarningControl {
        fail: AtomicBool::new(true),
        persisted_markers: Mutex::new(Vec::new()),
    };
    // When cancellation attempts its terminal checkpoint.
    assert!(worker::process_page(&state, &mut job, &control)
        .await
        .is_err());
    // Then no emitted-warning marker survives the failed commit.
    assert!(!job.progress.unknown_key_ids_warned);
    assert_eq!(*control.persisted_markers.lock().unwrap(), [true]);
}

#[tokio::test]
async fn warning_marker_is_committed_with_the_terminal_checkpoint() {
    // Given an execute job with unknown metadata and an available checkpoint.
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(mock().await), ring());
    let mut job = Job {
        id: uuid::Uuid::new_v4(),
        request: serde_json::from_value::<ObjectCreateJobRequest>(
            serde_json::json!({"mode":"execute"}),
        )
        .unwrap()
        .validated()
        .unwrap(),
        cursor: Cursor {
            pending_keys: vec!["object".into()],
            ..Cursor::default()
        },
        progress: Progress {
            unknown_key_ids: 2,
            ..Progress::default()
        },
    };
    let control = WarningControl {
        fail: AtomicBool::new(false),
        persisted_markers: Mutex::new(Vec::new()),
    };
    // When cancellation commits its terminal checkpoint.
    assert!(worker::process_page(&state, &mut job, &control)
        .await
        .unwrap());
    // Then the durable marker accompanies terminal progress before logging.
    assert!(job.progress.unknown_key_ids_warned);
    assert_eq!(*control.persisted_markers.lock().unwrap(), [true]);
}
