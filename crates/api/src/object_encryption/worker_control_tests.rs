use super::{
    lease::{Claim, Job},
    progress::{Cursor, Progress},
    types::ObjectCreateJobRequest,
    worker::{self, JobControl},
    worker_test_storage::{mock, ring},
    ObjectEncryptionState,
};
use database::DbPool;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
struct CancellingControl {
    cancelled: AtomicBool,
    persisted: AtomicUsize,
}
impl JobControl for CancellingControl {
    async fn check(&self) -> anyhow::Result<bool> {
        Ok(self.cancelled.load(Ordering::SeqCst))
    }
    async fn persist(&self, _: &mut Job, status: &str) -> anyhow::Result<()> {
        if status == "running" {
            self.persisted.fetch_add(1, Ordering::SeqCst);
            self.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_between_objects_preserves_the_completed_checkpoint() {
    let state = ObjectEncryptionState::new(DbPool::uninitialized(), Arc::new(mock().await), ring());
    let mut job = Job {
        id: uuid::Uuid::new_v4(),
        request: serde_json::from_value::<ObjectCreateJobRequest>(
            serde_json::json!({"mode":"execute","batch_size":1000}),
        )
        .unwrap()
        .validated()
        .unwrap(),
        cursor: Cursor {
            pending_keys: vec!["object".into(), "untouched".into()],
            ..Cursor::default()
        },
        progress: Progress::default(),
    };
    let control = CancellingControl {
        cancelled: AtomicBool::new(false),
        persisted: AtomicUsize::new(0),
    };
    assert!(worker::process_page(&state, &mut job, &control)
        .await
        .unwrap());
    assert_eq!(job.progress.processed, 1);
    assert_eq!(job.cursor.pending_keys, ["untouched"]);
    assert_eq!(control.persisted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn busy_lease_retries_until_it_becomes_available() {
    let calls = AtomicUsize::new(0);
    let outcome = worker::retry_busy(Duration::ZERO, || async {
        Ok(if calls.fetch_add(1, Ordering::SeqCst) < 2 {
            Claim::Busy
        } else {
            Claim::Stopped
        })
    })
    .await
    .unwrap();
    assert!(matches!(outcome, Claim::Stopped));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn busy_lease_retry_is_bounded() {
    let calls = AtomicUsize::new(0);
    let outcome = worker::retry_busy(Duration::ZERO, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Claim::Busy)
    })
    .await
    .unwrap();
    assert!(matches!(outcome, Claim::Busy));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        worker::CLAIM_RETRY_ATTEMPTS as usize + 1
    );
}
