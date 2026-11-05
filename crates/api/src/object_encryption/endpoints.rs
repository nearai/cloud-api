use super::{spawn_job, types::*, ObjectEncryptionState};
use crate::middleware::AdminUser;
use axum::{extract::Path, http::StatusCode, Extension, Json};
use serde_json::{json, Value};
use uuid::Uuid;

#[path = "scan.rs"]
mod scan_impl;

#[utoipa::path(get, path="/v1/admin/object-encryption/keys", tag="Admin",
    description="Admin session, read_write token, or read_only token. Reports object key IDs and fingerprints.",
    responses((status=200, description="Configured object write/read key IDs and fingerprints", body=Value)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn get_keys(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(_admin): Extension<AdminUser>,
) -> ApiResult<Value> {
    Ok(Json(json!({
        "write_key_id":state.ring.write_id(), "read_key_ids":state.ring.ids(),
        "fingerprints":state.ring.fingerprints().into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        "write_enabled":state.write_enabled
    })))
}

#[utoipa::path(post, path="/v1/admin/object-encryption/scan", tag="Admin", request_body=ObjectScanRequest,
    description="Admin session, read_write token, or read_only token. Inventories object metadata without modifying objects.",
    responses((status=200, description="Object encryption metadata inventory", body=Value)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn scan(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(_admin): Extension<AdminUser>,
    Json(req): Json<ObjectScanRequest>,
) -> ApiResult<Value> {
    scan_impl::run(&state, req).await
}

#[utoipa::path(post, path="/v1/admin/object-encryption/jobs", tag="Admin", request_body=ObjectCreateJobRequest,
    description="Admin session or read_write token. A read_only token receives HTTP 403 with error.type insufficient_permissions.",
    responses((status=202, description="Resumable object encryption job accepted", body=ObjectJobResponse), (status=409, description="An object encryption job with this scope is already queued or running", body=Value), (status=403, description="Insufficient permissions", body=crate::models::ErrorResponse)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn create_job(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<ObjectCreateJobRequest>,
) -> AcceptedJob {
    let req = req.validated()?;
    if matches!(req.mode, Mode::Execute) && !state.write_enabled {
        return Err(bad("object rewrap write gate is disabled"));
    }
    let id = Uuid::new_v4();
    let mode = req.mode.as_str();
    let scope = serde_json::to_value(req.scope).map_err(internal)?;
    let actions = json!(req.actions);
    let client = state.pool.get().await.map_err(internal)?;
    client.execute(
        "INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,admin_actor) VALUES($1,$2,'queued',$3,$4,$5,$6,$7)",
        &[&id, &mode, &scope, &actions, &req.batch_size, &req.max_rows, &admin.0.id]
    ).await.map_err(|error| {
        if error.as_db_error().is_some_and(|error| {
            error.code() == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
                && error.constraint() == Some("idx_database_encryption_jobs_active_scope")
        }) {
            (StatusCode::CONFLICT, Json(json!({"error": {
                "code": "conflict",
                "message": "an object encryption job is already queued or running"
            }})))
        } else {
            internal(error)
        }
    })?;
    drop(client);
    spawn_job(state.clone(), id);
    Ok((StatusCode::ACCEPTED, get_inner(&state, id).await?))
}

#[utoipa::path(get, path="/v1/admin/object-encryption/jobs/{id}", tag="Admin", params(("id"=Uuid, Path, description="Object job ID")),
    description="Admin session, read_write token, or read_only token. Reports an object job and its persisted progress.",
    responses((status=200, description="Object encryption job", body=ObjectJobResponse)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn get_job(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(_admin): Extension<AdminUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<ObjectJobResponse> {
    get_inner(&state, id).await
}

async fn get_inner(state: &ObjectEncryptionState, id: Uuid) -> ApiResult<ObjectJobResponse> {
    let client = state.pool.get().await.map_err(internal)?;
    let row = client.query_opt(
        "SELECT id,status,mode,scope,actions,progress,cursor,created_at,completed_at,last_error_class,last_error_message FROM database_encryption_jobs WHERE id=$1 AND scope->>'kind'='objects'",
        &[&id]
    ).await.map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(ObjectJobResponse {
        job_id: row.get(0),
        status: row.get(1),
        mode: row.get(2),
        scope: row.get(3),
        actions: row.get(4),
        progress: public_progress(row.get(5)),
        cursor: public_cursor(row.get(6)),
        created_at: row.get(7),
        completed_at: row.get(8),
        last_error_class: row.get(9),
        last_error_message: row.get(10),
    }))
}

#[utoipa::path(post, path="/v1/admin/object-encryption/jobs/{id}/cancel", tag="Admin", params(("id"=Uuid, Path, description="Object job ID")),
    description="Admin session or read_write token. A read_only token receives HTTP 403 with error.type insufficient_permissions.",
    responses((status=200, description="Cancellation requested", body=ObjectJobResponse), (status=403, description="Insufficient permissions", body=crate::models::ErrorResponse)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn cancel_job(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(_admin): Extension<AdminUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<ObjectJobResponse> {
    let client = state.pool.get().await.map_err(internal)?;
    let row = client.query_opt(
        "UPDATE database_encryption_jobs SET cancel_requested_at=NOW(),status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END,completed_at=CASE WHEN status='queued' THEN NOW() ELSE completed_at END WHERE id=$1 AND scope->>'kind'='objects' AND status IN ('queued','running') RETURNING id",
        &[&id]
    ).await.map_err(internal)?;
    drop(client);
    if row.is_none() {
        let _ = get_inner(&state, id).await?;
        return Err(bad("job is not cancellable"));
    }
    get_inner(&state, id).await
}

#[utoipa::path(post, path="/v1/admin/object-encryption/verify", tag="Admin", request_body=ObjectVerifyRequest,
    description="Admin session or read_write token. A read_only token receives HTTP 403 with error.type insufficient_permissions.",
    responses((status=202, description="Resumable object verification job accepted", body=ObjectJobResponse), (status=409, description="An object encryption job with this scope is already queued or running", body=Value), (status=403, description="Insufficient permissions", body=crate::models::ErrorResponse)), security(("session_token"=[]), ("admin_token"=[])))]
pub async fn verify(
    Extension(state): Extension<ObjectEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<ObjectVerifyRequest>,
) -> AcceptedJob {
    create_job(
        Extension(state),
        Extension(admin),
        Json(ObjectCreateJobRequest {
            mode: Mode::Verify,
            scope: req.scope,
            batch_size: req.batch_size.unwrap_or_else(batch_default),
            max_rows: None,
            actions: vec!["verify".into()],
            batch_delay_ms: req.batch_delay_ms,
        }),
    )
    .await
}

pub(super) fn public_progress(mut progress: Value) -> Value {
    for name in [
        "missing_keys",
        "changed_keys",
        "oversized_keys",
        "undecryptable_keys",
        "error_samples",
    ] {
        if let Some(samples) = progress.get_mut(name).and_then(Value::as_array_mut) {
            samples.truncate(20);
        }
    }
    if let Some(fields) = progress.as_object_mut() {
        fields.remove("unknown_key_ids_warned");
    }
    progress
}

pub(super) fn public_cursor(mut cursor: Value) -> Value {
    if let Some(fields) = cursor.as_object_mut() {
        for private in ["lease", "pending_keys", "pending_next"] {
            fields.remove(private);
        }
    }
    cursor
}
