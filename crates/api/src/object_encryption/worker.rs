use super::{
    progress::{Cursor, Progress},
    types::{Mode, ObjectCreateJobRequest, Scope},
    ObjectEncryptionState,
};
use crate::database_encryption::batch_delay;
use serde_json::{json, Value};
use services::files::FileServiceError;
use std::{future::Future, time::Duration};
use uuid::Uuid;

const OBJECT_WORKER_LOCK_KEY: i64 = 0x4e4541524f424a;
const STORAGE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn storage_call<T>(
    operation: impl Future<Output = Result<T, FileServiceError>>,
) -> anyhow::Result<T> {
    Ok(tokio::time::timeout(STORAGE_TIMEOUT, operation).await??)
}

enum BatchResult {
    Stopped,
    Continue(Option<Duration>),
}

pub(super) async fn run_job(state: &ObjectEncryptionState, id: Uuid) -> anyhow::Result<()> {
    let _permit = state.worker_permit.clone().acquire_owned().await?;
    loop {
        match run_batch(state, id).await? {
            BatchResult::Stopped => return Ok(()),
            BatchResult::Continue(delay) => {
                if batch_delay::wait_between_batches(delay.unwrap_or_default(), || {
                    batch_delay::cancel_if_requested(&state.pool, id)
                })
                .await?
                {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_batch(state: &ObjectEncryptionState, id: Uuid) -> anyhow::Result<BatchResult> {
    let mut client = state.pool.get().await?;
    let transaction = client.transaction().await?;
    transaction
        .batch_execute("SET LOCAL statement_timeout = '30s'")
        .await?;
    let locked: bool = transaction
        .query_one(
            "SELECT pg_try_advisory_xact_lock($1)",
            &[&OBJECT_WORKER_LOCK_KEY],
        )
        .await?
        .get(0);
    if !locked {
        transaction.rollback().await?;
        return Ok(BatchResult::Stopped);
    }
    let row = transaction.query_opt(
        "SELECT mode,scope,batch_size,max_rows,cursor,progress,actions,cancel_requested_at IS NOT NULL AS cancelled FROM database_encryption_jobs WHERE id=$1 AND scope->>'kind'='objects' AND status IN ('queued','running') FOR UPDATE",
        &[&id]
    ).await?;
    let Some(row) = row else {
        transaction.rollback().await?;
        return Ok(BatchResult::Stopped);
    };
    let cancelled: bool = row.get("cancelled");
    if cancelled {
        transaction.execute("UPDATE database_encryption_jobs SET status='cancelled',completed_at=NOW() WHERE id=$1 AND scope->>'kind'='objects'", &[&id]).await?;
        transaction.commit().await?;
        return Ok(BatchResult::Stopped);
    }
    let mode: Mode = serde_json::from_value(json!(row.get::<_, String>("mode")))?;
    let scope: Scope = serde_json::from_value(row.get("scope"))?;
    let request = ObjectCreateJobRequest {
        mode,
        scope,
        batch_size: row.get("batch_size"),
        max_rows: row.get("max_rows"),
        actions: serde_json::from_value(row.get("actions"))?,
        batch_delay_ms: None,
    }
    .validated()
    .map_err(|_| anyhow::anyhow!("invalid persisted object job"))?;
    let mut cursor: Cursor = serde_json::from_value(row.get("cursor"))?;
    let mut progress: Progress = serde_json::from_value(row.get("progress"))?;
    progress.retain_ring_keys(&state.ring);
    let remaining = request
        .max_rows
        .map(|max| (max - progress.processed).max(0));
    let cap = remaining
        .map(|remaining| remaining.min(request.batch_size))
        .unwrap_or(request.batch_size);
    if !cursor.complete && cap > 0 {
        let (keys, next) = storage_call(
            state
                .storage
                .list_keys(cursor.continuation.clone(), i32::try_from(cap)?),
        )
        .await?;
        anyhow::ensure!(
            i64::try_from(keys.len())? <= cap,
            "invalid object page size"
        );
        anyhow::ensure!(
            next.is_none() || next != cursor.continuation,
            "object pagination did not advance"
        );
        for key in &keys {
            process_key(state, request.mode, key, &mut progress).await?;
        }
        cursor.last_key = keys.last().cloned().or(cursor.last_key);
        cursor.complete = next.is_none();
        cursor.continuation = next;
    }
    let finished = cursor.complete
        || request
            .max_rows
            .is_some_and(|max| progress.processed >= max);
    progress.complete = cursor.complete;
    if finished && matches!(request.mode, Mode::Verify) {
        progress.pass = Some(cursor.complete && progress.stale == 0 && progress.undecryptable == 0);
    }
    let status = if finished { "completed" } else { "running" };
    let progress: Value = serde_json::to_value(progress)?;
    let cursor: Value = serde_json::to_value(cursor)?;
    transaction.execute(
        "UPDATE database_encryption_jobs SET status=$2,started_at=COALESCE(started_at,NOW()),completed_at=CASE WHEN $2='completed' THEN NOW() ELSE completed_at END,progress=$3,cursor=$4 WHERE id=$1 AND scope->>'kind'='objects'",
        &[&id, &status, &progress, &cursor]
    ).await?;
    transaction.commit().await?;
    if finished {
        return Ok(BatchResult::Stopped);
    }
    let delay = request
        .scope
        .options
        .batch_delay_ms
        .map(u64::try_from)
        .transpose()?
        .map(Duration::from_millis);
    Ok(BatchResult::Continue(delay))
}

pub(super) async fn process_key(
    state: &ObjectEncryptionState,
    mode: Mode,
    key: &str,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    let result = match mode {
        Mode::DryRun => storage_call(state.storage.object_key_id(key))
            .await
            .map(|key_id| progress.record_key_id(key_id, &state.ring)),
        Mode::Execute => storage_call(state.storage.rewrap_object(key))
            .await
            .map(|outcome| progress.record_rewrap(key, outcome)),
        Mode::Verify => storage_call(state.storage.verify_object(key))
            .await
            .map(|result| progress.record_verification(key, result)),
    };
    match result {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.downcast_ref::<FileServiceError>(),
                Some(FileServiceError::NotFound)
            ) =>
        {
            progress.record_missing(key);
        }
        Err(error) => return Err(error),
    }
    progress.processed += 1;
    progress.total += 1;
    Ok(())
}
