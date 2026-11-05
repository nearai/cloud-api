use super::{
    lease::{self, Claim, Job},
    types::Mode,
    ObjectEncryptionState,
};
use std::{future::Future, time::Duration};
use uuid::Uuid;

pub(super) use super::object_processing::{process_key, storage_call};

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
    for _ in 0..20 {
        match claim().await? {
            Claim::Busy => tokio::time::sleep(interval).await,
            outcome => return Ok(outcome),
        }
    }
    claim().await
}

pub(super) async fn run_job(state: &ObjectEncryptionState, id: Uuid) -> anyhow::Result<()> {
    let _permit = state.worker_permit.clone().acquire_owned().await?;
    let Claim::Owned(mut job) =
        retry_busy(state.lease_retry_interval, || lease::claim(state, id)).await?
    else {
        return Ok(());
    };
    let token = job
        .cursor
        .lease
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing object job lease"))?
        .token;
    let control = DatabaseControl { state, id, token };
    let result = run_owned(state, &mut job, &control).await;
    if result.is_err() {
        let warn = mark_unknown(&mut job);
        if lease::fail(state, &job).await? && warn {
            emit_unknown(&job);
        }
    }
    result
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
    loop {
        if control.check().await? {
            return finish(control, job, "cancelled").await;
        }
        anyhow::ensure!(
            !matches!(job.request.mode, Mode::Execute) || state.write_enabled,
            "object rewrap write gate is disabled"
        );
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
                    tracing::warn!(
                        error_class = "object_listing_unavailable",
                        "Object encryption listing will retry during recovery"
                    );
                    return control.persist(job, "queued").await;
                }
                Err(error) => return Err(error),
            };
            anyhow::ensure!(
                i64::try_from(keys.len())? <= cap,
                "invalid object page size"
            );
            anyhow::ensure!(
                next.is_none() || next != job.cursor.continuation,
                "object pagination did not advance"
            );
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
