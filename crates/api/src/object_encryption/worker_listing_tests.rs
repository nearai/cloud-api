use super::*;
use crate::object_encryption::{
    progress::{Cursor, Progress},
    types::ObjectCreateJobRequest,
    worker_test_storage::{fault, Failure},
};
use std::sync::{atomic::Ordering, Mutex};

#[derive(Default)]
struct RecordingControl(Mutex<Vec<(String, serde_json::Value)>>);
impl JobControl for RecordingControl {
    async fn check(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn persist(&self, job: &mut Job, status: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((status.into(), serde_json::to_value(&job.cursor)?));
        Ok(())
    }
}
fn job() -> Job {
    Job {
        id: Uuid::new_v4(),
        request: serde_json::from_value::<ObjectCreateJobRequest>(
            serde_json::json!({"mode":"dry_run"}),
        )
        .unwrap()
        .validated()
        .unwrap(),
        cursor: Cursor::default(),
        progress: Progress::default(),
    }
}
#[tokio::test]
async fn permanent_listing_failure_is_terminal_and_sanitized() {
    let (state, attempts) = fault(Failure::ListingPermanent).await;
    let mut job = job();
    let control = RecordingControl::default();
    run_owned(&state, &mut job, &control).await.unwrap();
    let checkpoints = control.0.lock().unwrap();
    assert_eq!(checkpoints.last().unwrap().0, "failed");
    assert_eq!(
        checkpoints.last().unwrap().1["last_error"],
        "object_listing_failed"
    );
    assert!(!serde_json::to_string(&checkpoints[0])
        .unwrap()
        .contains("private"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn consecutive_listing_failures_survive_cursor_resumption_and_eventually_fail() {
    let (state, attempts) = fault(Failure::ListingTransient(20)).await;
    let mut job = job();
    let control = RecordingControl::default();
    for attempt in 1..=LISTING_MAX_CONSECUTIVE_ATTEMPTS {
        run_owned(&state, &mut job, &control).await.unwrap();
        let value = serde_json::to_value(&job.cursor).unwrap();
        assert_eq!(value["listing_attempts"], attempt);
        job.cursor = serde_json::from_value(value).unwrap();
        let checkpoints = control.0.lock().unwrap();
        assert_eq!(
            checkpoints.last().unwrap().0,
            if attempt < LISTING_MAX_CONSECUTIVE_ATTEMPTS {
                "queued"
            } else {
                "failed"
            }
        );
        assert_eq!(
            checkpoints.last().unwrap().1["last_error"],
            if attempt < LISTING_MAX_CONSECUTIVE_ATTEMPTS {
                "object_listing_unavailable"
            } else {
                "object_listing_attempts_exhausted"
            }
        );
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        LISTING_MAX_CONSECUTIVE_ATTEMPTS as usize
    );
}
#[tokio::test]
async fn successful_listing_resets_the_consecutive_attempt_budget() {
    let (state, _) = fault(Failure::ListingTransient(1)).await;
    let mut job = job();
    let control = RecordingControl::default();
    run_owned(&state, &mut job, &control).await.unwrap();
    assert_eq!(
        serde_json::to_value(&job.cursor).unwrap()["listing_attempts"],
        1
    );
    run_owned(&state, &mut job, &control).await.unwrap();
    assert_eq!(
        serde_json::to_value(&job.cursor).unwrap()["listing_attempts"],
        0
    );
    assert_eq!(control.0.lock().unwrap().last().unwrap().0, "completed");
}

struct BrokenControl;
impl JobControl for BrokenControl {
    async fn check(&self) -> anyhow::Result<bool> {
        anyhow::bail!("original checkpoint failure")
    }
    async fn persist(&self, job: &mut Job, status: &str) -> anyhow::Result<()> {
        assert_eq!(status, "failed");
        assert_eq!(
            serde_json::to_value(&job.cursor).unwrap()["last_error"],
            "object_worker_failed"
        );
        anyhow::bail!("secondary failure-write error")
    }
}
#[tokio::test]
async fn failure_recording_does_not_replace_the_original_worker_error() {
    let (state, _) = fault(Failure::ListingPermanent).await;
    let mut job = job();
    let error = run_owned(&state, &mut job, &BrokenControl)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "original checkpoint failure");
    assert_eq!(
        serde_json::to_value(&job.cursor).unwrap()["last_error"],
        "object_worker_failed"
    );
}
