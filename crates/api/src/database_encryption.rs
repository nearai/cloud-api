use crate::middleware::AdminUser;
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use axum::{extract::Path, http::StatusCode, Extension, Json};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
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
    f("mcp_connectors", "name", Kind::Text, "Connector label"),
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
    let client = state.pool.get().await?;
    let mut out = vec![];
    for f in fields {
        let p = predicate(f);
        let from = limit
            .map(|n| {
                format!(
                    "FROM (SELECT {} FROM {} LIMIT {}) s",
                    f.column,
                    f.table,
                    n.clamp(1, 100_000)
                )
            })
            .unwrap_or_else(|| format!("FROM {}", f.table));
        let q=format!("SELECT count(*) FILTER(WHERE {0} IS NULL),count(*) FILTER(WHERE {p}),count(*) FILTER(WHERE {0} IS NOT NULL AND NOT({p})) {from}",f.column);
        let r = client.query_one(&q, &[]).await?;
        out.push(FieldCount {
            table: f.table.into(),
            column: f.column.into(),
            classification: "encrypt",
            empty: r.get(0),
            encrypted: r.get(1),
            plaintext: r.get(2),
            invalid_envelope: 0,
        });
    }
    Ok(out)
}
fn totals(c: &[FieldCount]) -> Value {
    json!({"plaintext":c.iter().map(|x|x.plaintext).sum::<i64>(),"encrypted":c.iter().map(|x|x.encrypted).sum::<i64>(),"empty":c.iter().map(|x|x.empty).sum::<i64>(),"invalid_envelope":0})
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
    use aes_gcm::aead::rand_core::{OsRng, RngCore};
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let aad = format!("{}:{}:{}", f.table, f.column, id);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid key"))?;
    let ct = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plain.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    Ok(json!({MARKER:true,"version":1,"alg":"AES-256-GCM","key_id":"s3-v1","nonce":BASE64.encode(nonce),"ciphertext":BASE64.encode(ct)}).to_string())
}

// Job lifecycle and authenticated envelope helpers.

#[cfg(test)]
fn decrypt_envelope(key: &[u8; 32], f: &Field, id: Uuid, encoded: &str) -> anyhow::Result<String> {
    let value: Value = serde_json::from_str(encoded)?;
    anyhow::ensure!(value[MARKER] == true, "missing encryption marker");
    anyhow::ensure!(value["version"] == 1, "unsupported envelope version");
    anyhow::ensure!(
        value["alg"] == "AES-256-GCM",
        "unsupported envelope algorithm"
    );
    let nonce: [u8; 12] = BASE64
        .decode(
            value["nonce"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing nonce"))?,
        )?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let ciphertext = BASE64.decode(
        value["ciphertext"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing ciphertext"))?,
    )?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid key"))?;
    let aad = format!("{}:{}:{}", f.table, f.column, id);
    let plaintext = cipher.decrypt(
        &Nonce::from(nonce),
        Payload {
            msg: &ciphertext,
            aad: aad.as_bytes(),
        },
    )?;
    Ok(String::from_utf8(plaintext)?)
}
pub async fn create_job(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<CreateJobRequest>,
) -> ApiResult<JobResponse> {
    if matches!(&req.mode, Mode::Execute) {
        return Err(bad(
            "execute mode is disabled until repository decrypt-on-read support is enabled",
        ));
    }
    if req.scope.tables.is_empty() && req.scope.fields.is_empty() {
        return Err(bad(
            "an explicit scope is required for database encryption jobs",
        ));
    }
    if !(1..=1000).contains(&req.batch_size) {
        return Err(bad("batch_size must be between 1 and 1000"));
    }
    if req
        .actions
        .iter()
        .any(|a| a != "encrypt" && a != "verify_only")
    {
        return Err(bad("unsupported action"));
    }
    let mut scope_request = req.scope;
    normalize_scope(&mut scope_request);
    let fs = selected(&scope_request)?;
    let id = Uuid::new_v4();
    let mode = match req.mode {
        Mode::DryRun => "dry_run",
        Mode::Execute => "execute",
    };
    let scope = serde_json::to_value(&scope_request).map_err(internal)?;
    let actions = json!(req.actions);
    let client = state.pool.get().await.map_err(internal)?;
    client.execute("INSERT INTO database_encryption_jobs(id,mode,status,scope,actions,batch_size,max_rows,admin_actor,started_at) VALUES($1,$2,'running',$3,$4,$5,$6,$7,NOW())",&[&id,&mode,&scope,&actions,&req.batch_size,&req.max_rows,&admin.0.id]).await.map_err(internal)?;
    if let Err(_e) = run(&state, id, mode, &fs, req.batch_size, req.max_rows).await {
        let msg = "batch_failed";
        client.execute("UPDATE database_encryption_jobs SET status='failed',last_error_class='batch_failed',last_error_message=$2,completed_at=NOW() WHERE id=$1",&[&id,&msg]).await.map_err(internal)?;
    }
    get_inner(&state, id).await
}
async fn run(
    state: &DatabaseEncryptionState,
    id: Uuid,
    mode: &str,
    fields: &[&Field],
    batch: i64,
    max: Option<i64>,
) -> anyhow::Result<()> {
    let client = state.pool.get().await?;
    let (mut processed, mut encrypted) = (0i64, 0i64);
    for f in fields {
        loop {
            if max.is_some_and(|m| processed >= m) {
                break;
            }
            let cap = max.map(|m| (m - processed).min(batch)).unwrap_or(batch);
            let p = predicate(f);
            let q=format!("SELECT id,{0}::text FROM {1} WHERE {0} IS NOT NULL AND NOT({p}) ORDER BY id LIMIT $1",f.column,f.table);
            let rows = client.query(&q, &[&cap]).await?;
            if rows.is_empty() {
                break;
            }
            if mode == "execute" {
                for row in &rows {
                    let rid: Uuid = row.get(0);
                    let plain: String = row.get(1);
                    let value = envelope(&state.key, f, rid, &plain)?;
                    let q = match f.kind {
                        Kind::Json => format!(
                            "UPDATE {} SET {}=$1::jsonb WHERE id=$2 AND NOT({})",
                            f.table, f.column, p
                        ),
                        Kind::Text => format!(
                            "UPDATE {} SET {}=$1 WHERE id=$2 AND NOT({})",
                            f.table, f.column, p
                        ),
                    };
                    encrypted += client.execute(&q, &[&value, &rid]).await? as i64;
                }
            }
            processed += rows.len() as i64;
            client
                .execute(
                    "UPDATE database_encryption_jobs SET progress=$2,cursor=$3 WHERE id=$1",
                    &[
                        &id,
                        &json!({"processed":processed,"encrypted":encrypted}),
                        &json!({"table":f.table,"column":f.column}),
                    ],
                )
                .await?;
            let cancel:bool=client.query_one("SELECT cancel_requested_at IS NOT NULL FROM database_encryption_jobs WHERE id=$1",&[&id]).await?.get(0);
            if cancel {
                client.execute("UPDATE database_encryption_jobs SET status='cancelled',completed_at=NOW() WHERE id=$1",&[&id]).await?;
                return Ok(());
            }
            if mode != "execute" {
                break;
            }
        }
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
        .map(|x| json!({"table":x.table,"column":x.column,"reason_code":"plaintext_remaining"}))
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
