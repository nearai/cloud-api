use super::{
    progress::{Cursor, Progress},
    types::{Mode, ObjectCreateJobRequest},
    ObjectEncryptionState,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

// A short advisory transaction serializes bucket-wide claims only. Storage runs
// after commit; expiring JSON leases and UUID-fenced writes protect recovery.
const BUCKET_LOCK: i64 = 0x4e4541524f424a;
const LEASE_SECONDS: i64 = 120;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Lease {
    pub token: Uuid,
    pub expires_at: i64,
}

pub struct Job {
    pub(super) id: Uuid,
    pub(super) request: ObjectCreateJobRequest,
    pub(super) cursor: Cursor,
    pub(super) progress: Progress,
}

pub enum Claim {
    Owned(Box<Job>),
    Busy,
    Stopped,
}

pub async fn claim(state: &ObjectEncryptionState, id: Uuid) -> anyhow::Result<Claim> {
    let mut client = state.pool.get().await?;
    let tx = client.transaction().await?;
    let locked: bool = tx
        .query_one(
            "SELECT pg_try_advisory_xact_lock($1::bigint)",
            &[&BUCKET_LOCK],
        )
        .await?
        .get(0);
    if !locked {
        return Ok(Claim::Busy);
    }
    let busy: bool = tx.query_one(
        "SELECT EXISTS(SELECT 1 FROM database_encryption_jobs WHERE scope->>'kind'='objects' AND status='running' AND ($1::boolean = false OR id=$2::uuid) AND (cursor->'lease'->>'expires_at')::bigint > EXTRACT(EPOCH FROM clock_timestamp())::bigint)", &[&state.job_scoped_leases, &id]
    ).await?.get(0);
    if busy {
        return Ok(Claim::Busy);
    }
    let Some(row) = tx.query_opt(
        "SELECT mode,scope,batch_size,max_rows,cursor,progress,actions,cancel_requested_at IS NOT NULL AS cancelled FROM database_encryption_jobs WHERE id=$1::uuid AND scope->>'kind'='objects' AND status IN ('queued','running') FOR UPDATE", &[&id]
    ).await? else { return Ok(Claim::Stopped) };
    if row.get::<_, bool>("cancelled") {
        tx.execute(
            "UPDATE database_encryption_jobs SET status='cancelled',completed_at=NOW() WHERE id=$1::uuid",
            &[&id],
        )
        .await?;
        tx.commit().await?;
        return Ok(Claim::Stopped);
    }
    let parsed = (|| -> anyhow::Result<Job> {
        anyhow::ensure!(state.max_bytes > 0, "invalid object size limit");
        let mode: Mode = serde_json::from_value(json!(row.get::<_, String>("mode")))?;
        anyhow::ensure!(
            !matches!(mode, Mode::Execute) || state.write_enabled,
            "object rewrap write gate is disabled"
        );
        let request = ObjectCreateJobRequest {
            mode,
            scope: serde_json::from_value(row.get("scope"))?,
            batch_size: row.get("batch_size"),
            max_rows: row.get("max_rows"),
            actions: serde_json::from_value(row.get("actions"))?,
            batch_delay_ms: None,
        }
        .validated()
        .map_err(|_| anyhow::anyhow!("invalid persisted object job"))?;
        let cursor: Cursor = serde_json::from_value(row.get("cursor"))?;
        let mut progress: Progress = serde_json::from_value(row.get("progress"))?;
        progress.retain_ring_keys(&state.ring);
        Ok(Job {
            id,
            request,
            cursor,
            progress,
        })
    })();
    let mut job = match parsed {
        Ok(job) => job,
        Err(_) => {
            tx.execute("UPDATE database_encryption_jobs SET status='failed',cursor=cursor-'lease',last_error_class='invalid_object_job',last_error_message='invalid_object_job',completed_at=NOW() WHERE id=$1::uuid", &[&id]).await?;
            tx.commit().await?;
            return Ok(Claim::Stopped);
        }
    };
    let expires_at: i64 = tx
        .query_one(
            "SELECT EXTRACT(EPOCH FROM clock_timestamp())::bigint + $1::bigint",
            &[&LEASE_SECONDS],
        )
        .await?
        .get(0);
    job.cursor.lease = Some(Lease {
        token: Uuid::new_v4(),
        expires_at,
    });
    tx.execute("UPDATE database_encryption_jobs SET status='running',started_at=COALESCE(started_at,NOW()),cursor=$2::jsonb WHERE id=$1::uuid", &[&id, &serde_json::to_value(&job.cursor)?]).await?;
    tx.commit().await?;
    Ok(Claim::Owned(Box::new(job)))
}

pub async fn check(state: &ObjectEncryptionState, id: Uuid, token: Uuid) -> anyhow::Result<bool> {
    let client = state.pool.get().await?;
    let row = client.query_opt(
        "UPDATE database_encryption_jobs SET cursor=jsonb_set(cursor,'{lease,expires_at}',to_jsonb(EXTRACT(EPOCH FROM clock_timestamp())::bigint + $3::bigint)) WHERE id=$1::uuid AND scope->>'kind'='objects' AND status='running' AND cursor->'lease'->>'token'=$2::text AND (cursor->'lease'->>'expires_at')::bigint > EXTRACT(EPOCH FROM clock_timestamp())::bigint RETURNING cancel_requested_at IS NOT NULL", &[&id, &token.to_string(), &LEASE_SECONDS]
    ).await?;
    let row = row.ok_or_else(|| anyhow::anyhow!("object job lease lost"))?;
    Ok(row.get(0))
}

pub async fn persist(
    state: &ObjectEncryptionState,
    job: &mut Job,
    status: &str,
) -> anyhow::Result<()> {
    let id = job.id;
    let lease = job
        .cursor
        .lease
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing object job lease"))?;
    let token = lease.token;
    let client = state.pool.get().await?;
    let updated = client.execute(
        "UPDATE database_encryption_jobs SET status=$3::text,progress=$4::jsonb,cursor=CASE WHEN $3::text='running' THEN jsonb_set($5::jsonb,'{lease,expires_at}',to_jsonb(EXTRACT(EPOCH FROM clock_timestamp())::bigint + $6::bigint)) ELSE $5::jsonb-'lease' END,completed_at=CASE WHEN $3::text IN ('completed','cancelled') THEN NOW() ELSE completed_at END WHERE id=$1::uuid AND scope->>'kind'='objects' AND status='running' AND cursor->'lease'->>'token'=$2::text AND (cursor->'lease'->>'expires_at')::bigint > EXTRACT(EPOCH FROM clock_timestamp())::bigint", &[&id, &token.to_string(), &status, &serde_json::to_value(&job.progress)?, &serde_json::to_value(&job.cursor)?, &LEASE_SECONDS]
    ).await?;
    anyhow::ensure!(updated == 1, "object job lease lost");
    Ok(())
}

pub async fn fail(state: &ObjectEncryptionState, job: &Job) -> anyhow::Result<bool> {
    let token = job
        .cursor
        .lease
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing object job lease"))?
        .token;
    let client = state.pool.get().await?;
    let updated = client.execute(
        "UPDATE database_encryption_jobs SET status='failed',cursor=$4::jsonb-'lease',progress=$3::jsonb,last_error_class='object_worker_failed',last_error_message='object_worker_failed',completed_at=NOW() WHERE id=$1::uuid AND scope->>'kind'='objects' AND status='running' AND cursor->'lease'->>'token'=$2::text AND (cursor->'lease'->>'expires_at')::bigint > EXTRACT(EPOCH FROM clock_timestamp())::bigint", &[&job.id, &token.to_string(), &serde_json::to_value(&job.progress)?, &serde_json::to_value(&job.cursor)?]
    ).await?;
    Ok(updated == 1)
}
