use crate::middleware::AdminUser;
use axum::{extract::Path, http::StatusCode, Extension, Json};
use chrono::{DateTime, Utc};
use database::DbPool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

const MARKER: &str = "__near_db_encrypted";

// Confidential fields and API contracts.

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Field {
    table: &'static str,
    column: &'static str,
    kind: Kind,
    action: &'static str,
    reason: &'static str,
}

const fn f(table: &'static str, column: &'static str, kind: Kind, reason: &'static str) -> Field {
    Field {
        table,
        column,
        kind,
        action: "encrypt",
        reason,
    }
}

const FIELDS: &[Field] = &[
    f(
        "response_items",
        "item",
        Kind::Json,
        "Persisted transcript and tool payloads",
    ),
    f(
        "responses",
        "instructions",
        Kind::Text,
        "Prompt instructions",
    ),
    f(
        "responses",
        "metadata",
        Kind::Json,
        "User response metadata",
    ),
    f(
        "conversations",
        "metadata",
        Kind::Json,
        "Conversation title and metadata",
    ),
    f("files", "filename", Kind::Text, "User file metadata"),
    f("files", "storage_key", Kind::Text, "Private object pointer"),
    f("files", "content_type", Kind::Text, "File metadata"),
    f(
        "mcp_connectors",
        "description",
        Kind::Text,
        "Connector description",
    ),
    f(
        "mcp_connectors",
        "mcp_server_url",
        Kind::Text,
        "Private endpoint",
    ),
    f("mcp_connectors", "auth_config", Kind::Json, "Credentials"),
    f(
        "mcp_connectors",
        "error_message",
        Kind::Text,
        "Upstream error",
    ),
    f(
        "mcp_connectors",
        "capabilities",
        Kind::Json,
        "Private tool schemas",
    ),
    f(
        "mcp_connectors",
        "metadata",
        Kind::Json,
        "Connector metadata",
    ),
    f(
        "mcp_connector_usage",
        "request_payload",
        Kind::Json,
        "Tool request",
    ),
    f(
        "mcp_connector_usage",
        "response_payload",
        Kind::Json,
        "Tool response",
    ),
    f(
        "mcp_connector_usage",
        "error_message",
        Kind::Text,
        "Tool error",
    ),
];

#[derive(Clone)]
pub struct DatabaseEncryptionState {
    pub pool: DbPool,
    key: [u8; 32],
}

impl DatabaseEncryptionState {
    pub fn new(pool: DbPool, hex_key: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(hex_key)?;
        let len = bytes.len();
        let key = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("database encryption key must be 32 bytes, got {len}"))?;
        Ok(Self { pool, key })
    }

    pub fn recover_jobs(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let result = async {
                let client = state.pool.get().await?;
                let rows = client
                    .query(
                        "SELECT id FROM database_encryption_jobs WHERE status IN ('queued', 'running') ORDER BY created_at",
                        &[],
                    )
                    .await?;
                for row in rows {
                    spawn_job(state.clone(), row.get(0));
                }
                anyhow::Ok(())
            }
            .await;
            if result.is_err() {
                tracing::error!(
                    error_class = "database_encryption_job_recovery_failed",
                    "Failed to recover database encryption jobs"
                );
            }
        });
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct Scope {
    #[serde(default)]
    tables: Vec<String>,
    #[serde(default)]
    fields: Vec<FieldName>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FieldName {
    table: String,
    column: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ScanRequest {
    #[serde(default)]
    scope: Scope,
    limit: Option<i64>,
    #[serde(default)]
    include_approved_plaintext: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    DryRun,
    Execute,
}

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    mode: Mode,
    #[serde(default)]
    scope: Scope,
    #[serde(default = "batch_default")]
    batch_size: i64,
    max_rows: Option<i64>,
    #[serde(default = "actions_default")]
    actions: Vec<String>,
}

fn batch_default() -> i64 {
    100
}

fn actions_default() -> Vec<String> {
    vec!["encrypt".into()]
}

#[derive(Debug, Serialize)]
pub struct FieldCount {
    table: String,
    column: String,
    classification: &'static str,
    plaintext: i64,
    encrypted: i64,
    empty: i64,
    invalid_envelope: i64,
    scanned: i64,
    complete: bool,
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    run_id: Uuid,
    status: &'static str,
    fields: Vec<FieldCount>,
    totals: Value,
}

#[derive(Debug, Serialize)]
pub struct JobResponse {
    job_id: Uuid,
    status: String,
    mode: String,
    scope: Value,
    actions: Value,
    progress: Value,
    cursor: Value,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    last_error_class: Option<String>,
    last_error_message: Option<String>,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<Value>)>;

// Request validation and database inventory helpers.

fn bad(m: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"code":"invalid_request","message":m}})),
    )
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    let _ = e;
    tracing::error!(
        error_class = "database_encryption",
        "database encryption operation failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(
            json!({"error":{"code":"database_encryption_failed","message":"Database encryption operation failed"}}),
        ),
    )
}

fn selected(scope: &Scope) -> Result<Vec<&'static Field>, (StatusCode, Json<Value>)> {
    let known_tables = FIELDS
        .iter()
        .map(|f| f.table)
        .collect::<std::collections::HashSet<_>>();
    if scope
        .tables
        .iter()
        .any(|t| !known_tables.contains(t.as_str()))
    {
        return Err(bad("scope contains an unknown table"));
    }
    if scope.fields.iter().any(|x| {
        !FIELDS
            .iter()
            .any(|f| f.table == x.table && f.column == x.column)
    }) {
        return Err(bad("scope contains an unknown field"));
    }
    let v = FIELDS
        .iter()
        .filter(|f| {
            scope.tables.is_empty() && scope.fields.is_empty()
                || scope.tables.iter().any(|t| t == f.table)
                || scope
                    .fields
                    .iter()
                    .any(|x| x.table == f.table && x.column == f.column)
        })
        .collect::<Vec<_>>();
    if v.is_empty() {
        Err(bad(
            "scope does not contain a registered confidential field",
        ))
    } else {
        Ok(v)
    }
}

fn normalize_scope(scope: &mut Scope) {
    scope.tables.sort();
    scope.tables.dedup();
    scope
        .fields
        .sort_by(|left, right| (&left.table, &left.column).cmp(&(&right.table, &right.column)));
    scope
        .fields
        .dedup_by(|left, right| left.table == right.table && left.column == right.column);
}

fn predicate(f: &Field) -> String {
    match f.kind {
        Kind::Json => format!(
            "{0} IS NOT NULL AND jsonb_typeof({0})='object' AND {0} ? '{MARKER}'",
            f.column
        ),
        Kind::Text => format!(
            "{0} IS NOT NULL AND {0} LIKE '{{\"{MARKER}\":true,%'",
            f.column
        ),
    }
}

async fn counts(
    state: &DatabaseEncryptionState,
    fields: &[&Field],
    limit: Option<i64>,
) -> anyhow::Result<Vec<FieldCount>> {
    let mut client = state.pool.get().await?;
    let mut out = vec![];
    for f in fields {
        let scan_limit = limit.map(|value| value.clamp(1, 100_000));
        let max_id_query = format!("SELECT id FROM {} ORDER BY id DESC LIMIT 1", f.table);
        let max_id = client
            .query_opt(&max_id_query, &[])
            .await?
            .map(|row| row.get::<_, Uuid>(0));
        let mut count = FieldCount {
            table: f.table.into(),
            column: f.column.into(),
            classification: "encrypt",
            empty: 0,
            encrypted: 0,
            plaintext: 0,
            invalid_envelope: 0,
            scanned: 0,
            complete: true,
        };
        let Some(max_id) = max_id else {
            out.push(count);
            continue;
        };
        let mut after_id = Uuid::nil();

        loop {
            let remaining = scan_limit
                .map(|value| value - count.scanned)
                .unwrap_or(1_000);
            if remaining <= 0 {
                count.complete = false;
                break;
            }
            let page_size = remaining.min(1_000);
            let transaction = client.transaction().await?;
            transaction
                .batch_execute("SET LOCAL statement_timeout = '5s'")
                .await?;
            let query = format!(
                "SELECT id,{}::text FROM {} WHERE id>$1 AND id<=$2 ORDER BY id LIMIT $3",
                f.column, f.table
            );
            let rows = transaction
                .query(&query, &[&after_id, &max_id, &page_size])
                .await?;
            transaction.commit().await?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let id: Uuid = row.get(0);
                after_id = id;
                let raw: Option<String> = row.get(1);
                let Some(raw) = raw else {
                    count.empty += 1;
                    continue;
                };
                match serde_json::from_str::<Value>(&raw) {
                    Ok(value) if value[MARKER] == true => {
                        if decrypt_envelope(&state.key, f, id, &raw).is_ok() {
                            count.encrypted += 1;
                        } else {
                            count.invalid_envelope += 1;
                        }
                    }
                    _ => count.plaintext += 1,
                }
            }
            count.scanned += rows.len() as i64;
            if after_id == max_id {
                break;
            }
        }
        out.push(count);
    }
    Ok(out)
}

fn totals(c: &[FieldCount]) -> Value {
    json!({"plaintext":c.iter().map(|x|x.plaintext).sum::<i64>(),"encrypted":c.iter().map(|x|x.encrypted).sum::<i64>(),"empty":c.iter().map(|x|x.empty).sum::<i64>(),"invalid_envelope":c.iter().map(|x|x.invalid_envelope).sum::<i64>(),"scanned":c.iter().map(|x|x.scanned).sum::<i64>(),"complete":c.iter().all(|x|x.complete)})
}

// Admin scan and verification endpoints.
pub async fn scan(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(_): Extension<AdminUser>,
    Json(req): Json<ScanRequest>,
) -> ApiResult<ScanResponse> {
    let fs = selected(&req.scope)?;
    let cs = counts(&state, &fs, req.limit).await.map_err(internal)?;
    let t = totals(&cs);
    let _ = req.include_approved_plaintext;
    Ok(Json(ScanResponse {
        run_id: Uuid::new_v4(),
        status: "completed",
        fields: cs,
        totals: t,
    }))
}

fn envelope(key: &[u8; 32], f: &Field, id: Uuid, plain: &str) -> anyhow::Result<String> {
    database::field_encryption::encrypt(key, f.table, f.column, id, plain)
}

// Job lifecycle and authenticated envelope helpers.

fn decrypt_envelope(key: &[u8; 32], f: &Field, id: Uuid, encoded: &str) -> anyhow::Result<String> {
    database::field_encryption::decrypt(key, f.table, f.column, id, encoded)
}

pub async fn create_job(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<JobResponse>), (StatusCode, Json<Value>)> {
    if req.scope.tables.is_empty() && req.scope.fields.is_empty() {
        return Err(bad(
            "an explicit scope is required for database encryption jobs",
        ));
    }
    if matches!(req.mode, Mode::Execute) && !state.pool.encryption_write_enabled() {
        return Err(bad("execute jobs require DB_ENCRYPTION_WRITE_ENABLED=true"));
    }
    if !(1..=1000).contains(&req.batch_size) {
        return Err(bad("batch_size must be between 1 and 1000"));
    }
    if req.actions.as_slice() != ["encrypt"] {
        return Err(bad("actions must contain exactly one encrypt action"));
    }
    if req.max_rows.is_some_and(|max_rows| max_rows <= 0) {
        return Err(bad("max_rows must be greater than zero"));
    }
    let mut scope_request = req.scope;
    normalize_scope(&mut scope_request);
    selected(&scope_request)?;
    let id = Uuid::new_v4();
    let mode = match req.mode {
        Mode::DryRun => "dry_run",
        Mode::Execute => "execute",
    };
    let scope = serde_json::to_value(&scope_request).map_err(internal)?;
    let actions = json!(req.actions);
    let client = state.pool.get().await.map_err(internal)?;
    client.execute("INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,admin_actor) VALUES($1,$2,'queued',$3,$4,$5,$6,$7)",&[&id,&mode,&scope,&actions,&req.batch_size,&req.max_rows,&admin.0.id]).await.map_err(internal)?;
    drop(client);
    spawn_job(state.clone(), id);
    let response = get_inner(&state, id).await?;
    Ok((StatusCode::ACCEPTED, response))
}

fn spawn_job(state: DatabaseEncryptionState, id: Uuid) {
    tokio::spawn(async move {
        if run_job(&state, id).await.is_err() {
            if let Ok(client) = state.pool.get().await {
                let _ = client
                    .execute(
                        "UPDATE database_encryption_jobs SET status='failed',last_error_class='batch_failed',last_error_message='batch_failed',completed_at=NOW() WHERE id=$1 AND status IN ('queued','running')",
                        &[&id],
                    )
                    .await;
            }
            tracing::error!(
                job_id = %id,
                error_class = "database_encryption_batch_failed",
                "Database encryption job failed"
            );
        }
    });
}

fn advisory_lock_key(id: Uuid) -> i64 {
    i64::from_be_bytes(
        id.as_bytes()[..8]
            .try_into()
            .expect("UUID prefix is 8 bytes"),
    )
}

async fn run_job(state: &DatabaseEncryptionState, id: Uuid) -> anyhow::Result<()> {
    let mut client = state.pool.get().await?;
    let lock_key = advisory_lock_key(id);
    let locked: bool = client
        .query_one("SELECT pg_try_advisory_lock($1)", &[&lock_key])
        .await?
        .get(0);
    if !locked {
        return Ok(());
    }

    let result = run_locked_job(state, id, &mut client).await;
    let _ = client
        .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key])
        .await;
    result
}

async fn run_locked_job(
    state: &DatabaseEncryptionState,
    id: Uuid,
    client: &mut tokio_postgres::Client,
) -> anyhow::Result<()> {
    let job = client
        .query_opt(
            "UPDATE database_encryption_jobs SET status='running',started_at=COALESCE(started_at,NOW()) WHERE id=$1 AND status IN ('queued','running') RETURNING mode,scope,batch_size,max_rows,cursor,progress",
            &[&id],
        )
        .await?;
    let Some(job) = job else {
        return Ok(());
    };
    let mode: String = job.get("mode");
    if mode == "execute" && !state.pool.encryption_write_enabled() {
        anyhow::bail!("execute jobs require DB_ENCRYPTION_WRITE_ENABLED=true");
    }
    let scope: Scope = serde_json::from_value(job.get("scope"))?;
    let fields = selected(&scope).map_err(|_| anyhow::anyhow!("invalid persisted scope"))?;
    let batch: i64 = job.get("batch_size");
    let max: Option<i64> = job.get("max_rows");
    let cursor: Value = job.get("cursor");
    let progress: Value = job.get("progress");
    let mut field_index = cursor["field_index"].as_u64().unwrap_or(0) as usize;
    let mut after_id = cursor["after_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or(Uuid::nil());
    let mut processed = progress["processed"].as_i64().unwrap_or(0);
    let mut encrypted = progress["encrypted"].as_i64().unwrap_or(0);

    while field_index < fields.len() && max.is_none_or(|limit| processed < limit) {
        let field = fields[field_index];
        let cap = max
            .map(|limit| (limit - processed).min(batch))
            .unwrap_or(batch);
        let predicate = predicate(field);
        let transaction = client.transaction().await?;
        transaction
            .batch_execute("SET LOCAL statement_timeout = '30s'")
            .await?;
        let cancelled: bool = transaction
            .query_one(
                "SELECT cancel_requested_at IS NOT NULL FROM database_encryption_jobs WHERE id=$1",
                &[&id],
            )
            .await?
            .get(0);
        if cancelled {
            transaction
                .execute(
                    "UPDATE database_encryption_jobs SET status='cancelled',completed_at=NOW() WHERE id=$1",
                    &[&id],
                )
                .await?;
            transaction.commit().await?;
            return Ok(());
        }

        let locking_clause = if mode == "execute" { " FOR UPDATE" } else { "" };
        let query = format!(
            "SELECT id,{0}::text FROM {1} WHERE id>$1 AND {0} IS NOT NULL AND NOT({predicate}) ORDER BY id LIMIT $2{locking_clause}",
            field.column, field.table
        );
        let rows = transaction.query(&query, &[&after_id, &cap]).await?;
        if rows.is_empty() {
            field_index += 1;
            after_id = Uuid::nil();
        } else {
            let mut row_ids = Vec::with_capacity(rows.len());
            let mut encrypted_values = Vec::with_capacity(rows.len());
            for row in &rows {
                let row_id: Uuid = row.get(0);
                after_id = row_id;
                if mode == "execute" {
                    let plaintext: String = row.get(1);
                    row_ids.push(row_id);
                    encrypted_values.push(envelope(&state.key, field, row_id, &plaintext)?);
                }
            }
            if mode == "execute" {
                let value_expression = match field.kind {
                    Kind::Json => "batch.value::jsonb",
                    Kind::Text => "batch.value",
                };
                let update = format!(
                    "UPDATE {table} AS target SET {column}={value_expression} \
                     FROM UNNEST($1::uuid[], $2::text[]) AS batch(id,value) \
                     WHERE target.id=batch.id",
                    table = field.table,
                    column = field.column,
                );
                encrypted += transaction
                    .execute(&update, &[&row_ids, &encrypted_values])
                    .await? as i64;
            }
            processed += rows.len() as i64;
        }

        transaction
            .execute(
                "UPDATE database_encryption_jobs SET progress=$2,cursor=$3 WHERE id=$1",
                &[
                    &id,
                    &json!({"processed":processed,"encrypted":encrypted}),
                    &json!({"field_index":field_index,"after_id":after_id}),
                ],
            )
            .await?;
        transaction.commit().await?;
    }

    client
        .execute(
            "UPDATE database_encryption_jobs SET status='completed',completed_at=NOW() WHERE id=$1",
            &[&id],
        )
        .await?;
    Ok(())
}

pub async fn get_job(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(_): Extension<AdminUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<JobResponse> {
    get_inner(&state, id).await
}

async fn get_inner(state: &DatabaseEncryptionState, id: Uuid) -> ApiResult<JobResponse> {
    let c = state.pool.get().await.map_err(internal)?;
    let r=c.query_opt("SELECT id,status,mode,scope,actions,progress,cursor,created_at,completed_at,last_error_class,last_error_message FROM database_encryption_jobs WHERE id=$1",&[&id]).await.map_err(internal)?.ok_or_else(||(StatusCode::NOT_FOUND,Json(json!({"error":{"code":"job_not_found","message":"Database encryption job not found"}}))))?;
    Ok(Json(JobResponse {
        job_id: r.get(0),
        status: r.get(1),
        mode: r.get(2),
        scope: r.get(3),
        actions: r.get(4),
        progress: r.get(5),
        cursor: r.get(6),
        created_at: r.get(7),
        completed_at: r.get(8),
        last_error_class: r.get(9),
        last_error_message: r.get(10),
    }))
}

pub async fn cancel_job(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(_): Extension<AdminUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<JobResponse> {
    let c = state.pool.get().await.map_err(internal)?;
    let n=c.execute("UPDATE database_encryption_jobs SET cancel_requested_at=NOW(),status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END,completed_at=CASE WHEN status='queued' THEN NOW() ELSE completed_at END WHERE id=$1 AND status IN('queued','running')",&[&id]).await.map_err(internal)?;
    if n == 0 {
        return Err(bad("job is not cancellable"));
    }
    get_inner(&state, id).await
}

#[derive(Debug, Deserialize, Default)]
pub struct VerifyRequest {
    #[serde(default)]
    scope: Scope,
    #[serde(default = "yes")]
    fail_on_approved_plaintext_without_reason: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pass: bool,
    fields: Vec<FieldCount>,
    failing_fields: Vec<Value>,
}

pub async fn verify(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(_): Extension<AdminUser>,
    Json(req): Json<VerifyRequest>,
) -> ApiResult<VerifyResponse> {
    let fs = selected(&req.scope)?;
    let cs = counts(&state, &fs, None).await.map_err(internal)?;
    let fails = cs
        .iter()
        .filter(|x| x.plaintext > 0 || x.invalid_envelope > 0)
        .map(|x| {
            let reason = if x.invalid_envelope > 0 {
                "invalid_envelope"
            } else {
                "plaintext_remaining"
            };
            json!({"table":x.table,"column":x.column,"reason_code":reason})
        })
        .collect::<Vec<_>>();
    let _ = req.fail_on_approved_plaintext_without_reason;
    Ok(Json(VerifyResponse {
        pass: fails.is_empty(),
        fields: cs,
        failing_fields: fails,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_hides_plaintext() {
        let v = envelope(&[7; 32], &FIELDS[0], Uuid::nil(), "secret").unwrap();
        assert!(v.contains(MARKER));
        assert!(!v.contains("secret"));
        assert_eq!(
            decrypt_envelope(&[7; 32], &FIELDS[0], Uuid::nil(), &v).unwrap(),
            "secret"
        );
    }
    #[test]
    fn envelope_authenticates_context_and_key() {
        let v = envelope(&[7; 32], &FIELDS[0], Uuid::nil(), "secret").unwrap();
        assert!(decrypt_envelope(&[8; 32], &FIELDS[0], Uuid::nil(), &v).is_err());
        assert!(decrypt_envelope(&[7; 32], &FIELDS[1], Uuid::nil(), &v).is_err());
        assert!(decrypt_envelope(&[7; 32], &FIELDS[0], Uuid::new_v4(), &v).is_err());
    }

    #[test]
    fn malformed_envelope_is_rejected() {
        let malformed = json!({MARKER: true, "version": 1, "alg": "AES-256-GCM"}).to_string();
        assert!(decrypt_envelope(&[7; 32], &FIELDS[0], Uuid::nil(), &malformed).is_err());
    }
    #[test]
    fn key_requires_32_decoded_bytes() {
        assert!(hex::decode("not-hex").is_err());
        let short: Result<[u8; 32], _> = hex::decode("00").unwrap().try_into();
        assert!(short.is_err());
    }
    #[test]
    fn registry_unique() {
        let mut n = FIELDS
            .iter()
            .map(|f| (f.table, f.column))
            .collect::<Vec<_>>();
        n.sort();
        n.dedup();
        assert_eq!(n.len(), FIELDS.len());
    }
}
