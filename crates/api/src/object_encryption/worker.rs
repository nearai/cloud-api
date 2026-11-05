use super::{
    job_error::{transient, JobError},
    lease::{self, Claim, Job},
    types::Mode,
    ObjectEncryptionState,
};
use std::{future::Future, time::Duration};
use uuid::Uuid;

pub(super) use super::object_processing::{process_key, storage_call};
pub(super) const CLAIM_RETRY_ATTEMPTS: u32 = 20;
pub(super) const LISTING_MAX_CONSECUTIVE_ATTEMPTS: u32 = 5;

pub(super) trait JobControl: Sync {
    fn check(&self) -> impl Future<Output = anyhow::Result<bool>> + Send;
    fn persist(
        &self,
        job: &mut Job,
        status: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

struct DatabaseControl<'a> {
    state: &'a ObjectEncryptionState,
    id: Uuid,
    token: Uuid,
}

impl JobControl for DatabaseControl<'_> {
    async fn check(&self) -> anyhow::Result<bool> {
        lease::check(self.state, self.id, self.token).await
    }
    async fn persist(&self, job: &mut Job, status: &str) -> anyhow::Result<()> {
        lease::persist(self.state, job, status).await
    }
}

pub(super) async fn retry_busy<F, Fut>(interval: Duration, mut claim: F) -> anyhow::Result<Claim>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Claim>>,
{
    for _ in 0..CLAIM_RETRY_ATTEMPTS {
        match claim().await? {
            Claim::Busy => tokio::time::sleep(interval).await,
            outcome => return Ok(outcome),
        }
    }
    claim().await
}

pub(super) async fn run_job(state: &ObjectEncryptionState, id: Uuid) -> anyhow::Result<()> {
    let _permit = state.worker_permit.clone().acquire_owned().await?;
    match retry_busy(state.lease_retry_interval, || lease::claim(state, id)).await? {
        Claim::Owned(mut job) => {
            let token = job
                .cursor
                .lease
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing object job lease"))?
                .token;
            run_owned(state, &mut job, &DatabaseControl { state, id, token }).await
        }
        Claim::Busy => {
            tracing::warn!(job_id=%id, attempts=CLAIM_RETRY_ATTEMPTS + 1, "Object job claim retry budget exhausted");
            lease::record_busy(state, id).await
        }
        Claim::Stopped => Ok(()),
    }
}

async fn heartbeat<T>(
    control: &impl JobControl,
    operation: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::pin!(operation);
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.tick().await;
    loop {
        // Renew first after suspension; a lost lease drops the storage future
        // before the old owner can poll it again.
        tokio::select! {
            biased;
            _ = interval.tick() => { control.check().await?; }
            result = &mut operation => return result,
        }
    }
}

async fn run_owned(
    state: &ObjectEncryptionState,
    job: &mut Job,
    control: &impl JobControl,
) -> anyhow::Result<()> {
    let result = process_owned(state, job, control).await;
    if let Err(error) = &result {
        job.cursor.last_error = Some(JobError::from_error(error));
        // Preserve the original cause even if writing terminal status also fails.
        tracing::error!(job_id=%job.id, error=%error, "Object encryption worker failed");
        if let Err(failure_error) = finish(control, job, "failed").await {
            tracing::error!(job_id=%job.id, error=%failure_error, "Failed to record object worker failure");
        }
    }
    result
}

async fn process_owned(
    state: &ObjectEncryptionState,
    job: &mut Job,
    control: &impl JobControl,
) -> anyhow::Result<()> {
    loop {
        if control.check().await? {
            return finish(control, job, "cancelled").await;
        }
        if matches!(job.request.mode, Mode::Execute) && !state.write_enabled {
            return Err(JobError::WriteDisabled.into());
        }
        if job.cursor.pending_keys.is_empty() && !finished(job) {
            let cap = job
                .request
                .max_rows
                .map(|max| (max - job.progress.processed).min(job.request.batch_size))
                .unwrap_or(job.request.batch_size);
            let page = heartbeat(
                control,
                storage_call(
                    state
                        .storage
                        .list_keys(job.cursor.continuation.clone(), i32::try_from(cap)?),
                ),
            )
            .await;
            let (keys, next) = match page {
                Ok(page) => page,
                Err(error)
                    if error.is::<services::files::FileServiceError>()
                        || error.is::<tokio::time::error::Elapsed>() =>
                {
                    job.cursor.listing_attempts = job.cursor.listing_attempts.saturating_add(1);
                    let (failure, status) = if !transient(&error) {
                        (JobError::ListingFailed, "failed")
                    } else if job.cursor.listing_attempts >= LISTING_MAX_CONSECUTIVE_ATTEMPTS {
                        (JobError::ListingAttemptsExhausted, "failed")
                    } else {
                        (JobError::ListingUnavailable, "queued")
                    };
                    job.cursor.last_error = Some(failure);
                    tracing::warn!(job_id=%job.id, error_class=failure.class(), attempts=job.cursor.listing_attempts, "Object encryption listing failed");
                    return finish(control, job, status).await;
                }
                Err(error) => return Err(error),
            };
            if i64::try_from(keys.len())? > cap
                || (next.is_some() && next == job.cursor.continuation)
            {
                return Err(JobError::InvalidPage.into());
            }
            job.cursor.listing_attempts = 0;
            job.cursor.last_error = None;
            job.cursor.pending_keys = keys;
            job.cursor.pending_next = next;
            if job.cursor.pending_keys.is_empty() {
                advance_page(job);
            }
        }
        if process_page(state, job, control).await? {
            return Ok(());
        }
        if finished(job) {
            job.progress.complete = job.cursor.complete;
            if matches!(job.request.mode, Mode::Verify) {
                job.progress.pass = Some(job.progress.verification_passes());
            }
            return finish(control, job, "completed").await;
        }
        let delay = Duration::from_millis(u64::try_from(
            job.request.scope.options.batch_delay_ms.unwrap_or_default(),
        )?);
        heartbeat(control, async {
            crate::database_encryption::batch_delay::wait_between_batches(delay, || control.check())
                .await
        })
        .await?;
    }
}

pub(super) async fn process_page(
    state: &ObjectEncryptionState,
    job: &mut Job,
    control: &impl JobControl,
) -> anyhow::Result<bool> {
    while let Some(key) = job.cursor.pending_keys.first().cloned() {
        if control.check().await? {
            finish(control, job, "cancelled").await?;
            return Ok(true);
        }
        heartbeat(
            control,
            process_key(state, job.request.mode, &key, &mut job.progress),
        )
        .await?;
        job.cursor.last_key = Some(key);
        job.cursor.pending_keys.remove(0);
        if job.cursor.pending_keys.is_empty() {
            advance_page(job);
        }
        // Each completed object is checkpointed before starting another storage call.
        control.persist(job, "running").await?;
    }
    Ok(false)
}

fn finished(job: &Job) -> bool {
    job.cursor.complete
        || job
            .request
            .max_rows
            .is_some_and(|max| job.progress.processed >= max)
}

fn advance_page(job: &mut Job) {
    job.cursor.continuation = job.cursor.pending_next.take();
    job.cursor.complete = job.cursor.continuation.is_none();
}

async fn finish(control: &impl JobControl, job: &mut Job, status: &str) -> anyhow::Result<()> {
    let warn = mark_unknown(job);
    if let Err(error) = control.persist(job, status).await {
        if warn {
            job.progress.unknown_key_ids_warned = false;
        }
        return Err(error);
    }
    if warn {
        emit_unknown(job);
    }
    Ok(())
}

fn mark_unknown(job: &mut Job) -> bool {
    let warn = matches!(job.request.mode, Mode::Execute)
        && job.progress.unknown_key_ids > 0
        && !job.progress.unknown_key_ids_warned;
    if warn {
        job.progress.unknown_key_ids_warned = true;
    }
    warn
}

fn emit_unknown(job: &Job) {
    tracing::warn!(
        unknown_key_ids = job.progress.unknown_key_ids,
        "Object encryption job encountered unknown metadata key IDs"
    );
}

#[cfg(test)]
#[path = "worker_listing_tests.rs"]
mod listing_tests;
