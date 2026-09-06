use crate::middleware::AdminUser;
use axum::{extract::Path, http::StatusCode, Extension, Json};
use chrono::{DateTime, Utc};
use database::DbPool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const MARKER: &str = "__near_db_encrypted";
const MAX_BATCH_PLAINTEXT_BYTES: i64 = 8 * 1024 * 1024;

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

#[derive(Clone, Copy)]
struct ApprovedGroup {
    table: &'static str,
    columns: &'static [&'static str],
    reason: &'static str,
}

const APPROVED: &[ApprovedGroup] = &[
    ApprovedGroup { table: "admin_access_token", columns: &["token_hash", "name", "creation_reason", "revocation_reason", "user_agent"], reason: "Operational admin-token metadata; the credential itself is stored as a one-way hash" },
    ApprovedGroup { table: "aml_allowlisted_accounts", columns: &["account_id", "address_type", "reason"], reason: "Queryable compliance allowlist and audit rationale" },
    ApprovedGroup { table: "aml_reports", columns: &["flow", "provider", "account_id", "address_type", "risk_level", "report_id", "reason", "result_json"], reason: "Queryable compliance evidence with access restricted to AML/admin workflows" },
    ApprovedGroup { table: "api_keys", columns: &["key_hash", "name", "key_prefix"], reason: "API credentials are one-way hashed; name and prefix are query/display metadata" },
    ApprovedGroup { table: "chat_signatures", columns: &["chat_id", "text", "signature", "signing_address", "signing_algo", "signature_kind"], reason: "Publicly verifiable signature material required for lookup and verification" },
    ApprovedGroup { table: "database_encryption_jobs", columns: &["mode", "status", "scope", "actions", "cursor", "progress", "last_error_class", "last_error_message"], reason: "Non-customer operational migration state with redacted diagnostics" },
    ApprovedGroup { table: "feature_request_targets", columns: &["kind", "key", "title", "status"], reason: "Product catalog and workflow state" },
    ApprovedGroup { table: "feature_request_votes", columns: &["note", "source"], reason: "User-submitted product feedback intentionally available to administrators" },
    ApprovedGroup { table: "files", columns: &["purpose"], reason: "Queryable protocol enum; file-identifying fields are encrypted separately" },
    ApprovedGroup { table: "mcp_connector_usage", columns: &["method"], reason: "Queryable protocol method; request, response, and error payloads are encrypted" },
    ApprovedGroup { table: "mcp_connectors", columns: &["name", "auth_type", "connection_status"], reason: "Name is required by the per-organization uniqueness contract; remaining fields are protocol enums" },
    ApprovedGroup { table: "model_aliases", columns: &["alias_name"], reason: "Public model routing identifier" },
    ApprovedGroup { table: "model_deprecation_email_deliveries", columns: &["model_name", "model_display_name", "successor_model_name", "recipient_email", "organization_name", "status", "email_message_id", "email_last_error", "initiated_by_user_email"], reason: "Restricted operational email-delivery audit log" },
    ApprovedGroup { table: "model_pricing_change_email_deliveries", columns: &["recipient_email", "organization_name", "model_names", "status", "email_message_id", "email_last_error", "initiated_by_user_email"], reason: "Restricted operational email-delivery audit log" },
    ApprovedGroup { table: "models", columns: &["model_name", "model_display_name", "model_description", "model_icon", "owned_by", "provider_type", "provider_config", "input_modalities", "output_modalities", "inference_url", "hugging_face_id", "quantization", "supported_sampling_parameters", "supported_features", "datacenters", "attestation_policy", "openrouter_slug", "text_pricing"], reason: "Public or administrator-managed model catalog and routing configuration" },
    ApprovedGroup { table: "model_history", columns: &["model_display_name", "model_description", "change_reason", "model_name", "model_icon", "owned_by", "changed_by_user_email", "provider_type", "provider_config", "input_modalities", "output_modalities", "inference_url", "hugging_face_id", "quantization", "supported_sampling_parameters", "supported_features", "datacenters", "attestation_policy", "openrouter_slug", "text_pricing"], reason: "Administrator-visible model configuration audit history" },
    ApprovedGroup { table: "near_used_nonces", columns: &["nonce_hex"], reason: "Public-chain replay-prevention value" },
    ApprovedGroup { table: "oauth_states", columns: &["state", "provider", "pkce_verifier", "frontend_callback"], reason: "Short-lived OAuth handshake state required for indexed callback lookup and PKCE completion" },
    ApprovedGroup { table: "organization_invitations", columns: &["email", "role", "status", "token", "email_status", "email_last_error", "email_message_id"], reason: "Invitation workflow data required for indexed acceptance and delivery operations; tokens are short-lived" },
    ApprovedGroup { table: "organization_limits_history", columns: &["changed_by", "change_reason", "changed_by_user_email", "credit_type", "source", "currency"], reason: "Restricted billing and limits audit history" },
    ApprovedGroup { table: "organization_members", columns: &["role"], reason: "Queryable authorization role" },
    ApprovedGroup { table: "organization_reporting_tokens", columns: &["name", "token_hash", "token_prefix"], reason: "Reporting credentials are one-way hashed; name and prefix are display metadata" },
    ApprovedGroup { table: "organization_staking_farm_sources", columns: &["near_account_id", "network_id", "contract_id", "farm_product_id", "farm_price_id", "status", "sync_status", "last_sync_error", "active_positions"], reason: "Public-chain identifiers and restricted staking synchronization state" },
    ApprovedGroup { table: "organization_usage_log", columns: &["request_type", "model_name", "inference_type", "provider_request_id", "stop_reason", "served_provider_tier", "served_provider_type", "billing_details", "service_tier", "context_band"], reason: "Restricted metering and billing dimensions" },
    ApprovedGroup { table: "organizations", columns: &["name", "description", "settings"], reason: "Organization profile and administrator-managed settings" },
    ApprovedGroup { table: "refresh_tokens", columns: &["token_hash", "ip_address", "user_agent"], reason: "Refresh credentials are one-way hashed; security telemetry supports session management" },
    ApprovedGroup { table: "refinery_schema_history", columns: &["name", "applied_on", "checksum"], reason: "Database migration framework bookkeeping" },
    ApprovedGroup { table: "responses", columns: &["model", "status", "usage", "next_response_ids"], reason: "Queryable response lifecycle, routing, usage, and structural relationship data" },
    ApprovedGroup { table: "scheduled_model_pricing_changes", columns: &["model_name", "model_display_name", "status", "last_error", "cancelled_by_user_email", "created_by_user_email", "change_reason", "old_text_pricing", "new_text_pricing"], reason: "Restricted administrator pricing workflow and audit data" },
    ApprovedGroup { table: "services", columns: &["service_name", "display_name", "description", "unit"], reason: "Public service catalog" },
    ApprovedGroup { table: "users", columns: &["email", "username", "display_name", "avatar_url", "auth_provider", "provider_user_id"], reason: "Account identity fields required for login, uniqueness, and user-facing profiles" },
    ApprovedGroup { table: "workspaces", columns: &["name", "description", "settings"], reason: "Workspace profile and administrator-managed settings" },
];

#[derive(Clone, Copy)]
struct ExcludedTable {
    table: &'static str,
    reason: &'static str,
}

const EXCLUDED_TABLES: &[ExcludedTable] = &[ExcludedTable {
    table: "postgres_log",
    reason:
        "PostgreSQL-managed operational log table governed by infrastructure log-retention controls",
}];

fn excluded_table_reason(table: &str) -> Option<&'static str> {
    // Keep exclusions exact: a prefix such as `postgres_%` could hide a future
    // application table and prevent the classification gate from detecting it.
    EXCLUDED_TABLES
        .iter()
        .find(|excluded| excluded.table == table)
        .map(|excluded| excluded.reason)
}

fn approved_reason(table: &str, column: &str) -> Option<&'static str> {
    APPROVED
        .iter()
        .find(|group| group.table == table && group.columns.contains(&column))
        .map(|group| group.reason)
}

fn is_encrypted_field(table: &str, column: &str) -> bool {
    FIELDS
        .iter()
        .any(|field| field.table == table && field.column == column)
}

#[derive(Clone)]
pub struct DatabaseEncryptionState {
    pub pool: DbPool,
    key: [u8; 32],
    key_id: String,
    /// Database migrations are serialized so a backfill cannot consume the
    /// application's connection pool.
    worker_permit: Arc<Semaphore>,
    recovery_started: Arc<AtomicBool>,
    active_jobs: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    recovery_interval: std::time::Duration,
}

impl DatabaseEncryptionState {
    pub fn new(pool: DbPool, hex_key: &str, key_id: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(hex_key)?;
        let len = bytes.len();
        let key = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("database encryption key must be 32 bytes, got {len}"))?;
        database::field_encryption::validate_key_id(key_id)?;
        Ok(Self {
            pool,
            key,
            key_id: key_id.to_string(),
            worker_permit: Arc::new(Semaphore::new(1)),
            recovery_started: Arc::new(AtomicBool::new(false)),
            active_jobs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            recovery_interval: std::time::Duration::from_secs(30),
        })
    }

    #[doc(hidden)]
    pub fn with_recovery_interval(mut self, interval: std::time::Duration) -> Self {
        self.recovery_interval = interval;
        self
    }

    pub fn recover_jobs(&self) {
        if self.recovery_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(state.recovery_interval);
            loop {
                interval.tick().await;
                let result = async {
                    let client = state.pool.get().await?;
                    let rows = client
                        .query(
                            "SELECT id FROM database_encryption_jobs WHERE status IN ('queued', 'running') ORDER BY created_at",
                            &[],
                        )
                        .await?;
                    drop(client);
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    DryRun,
    Execute,
    Verify,
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
    approved_plaintext: Vec<Value>,
    excluded: Vec<Value>,
    unclassified: Vec<Value>,
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

async fn classify_schema(
    client: &tokio_postgres::Client,
) -> anyhow::Result<(Vec<Value>, Vec<Value>, Vec<Value>)> {
    let rows = client
        .query(
            "SELECT c.table_name,c.column_name FROM information_schema.columns c \
             JOIN information_schema.tables t ON t.table_schema=c.table_schema AND t.table_name=c.table_name \
             WHERE c.table_schema='public' AND t.table_type='BASE TABLE' \
             AND (c.data_type IN ('text','character varying','json','jsonb') OR c.data_type='ARRAY') \
             ORDER BY c.table_name,c.ordinal_position",
            &[],
        )
        .await?;
    let mut approved = Vec::new();
    let mut excluded = Vec::new();
    let mut unclassified = Vec::new();
    for row in rows {
        let table: String = row.get(0);
        let column: String = row.get(1);
        // The staging postgres_log relation is a BASE TABLE and therefore reaches
        // this branch. Keep the exclusion exact so similarly named application
        // tables remain subject to the classification gate.
        if let Some(reason) = excluded_table_reason(&table) {
            excluded.push(json!({
                "table": table,
                "column": column,
                "classification": "excluded",
                "reason": reason,
            }));
            continue;
        }
        if is_encrypted_field(&table, &column) {
            continue;
        }
        if let Some(reason) = approved_reason(&table, &column) {
            approved.push(json!({
                "table": table,
                "column": column,
                "classification": "approved_plaintext",
                "reason": reason,
            }));
        } else {
            unclassified.push(json!({
                "table": table,
                "column": column,
                "classification": "unclassified",
                "reason": "classification_required",
            }));
        }
    }
    Ok((approved, excluded, unclassified))
}

async fn schema_classification(
    state: &DatabaseEncryptionState,
) -> anyhow::Result<(Vec<Value>, Vec<Value>, Vec<Value>)> {
    let client = state.pool.get().await?;
    classify_schema(&client).await
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
                        if decrypt_envelope(&state.key, &state.key_id, f, id, &raw).is_ok() {
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
    let (approved, excluded, unclassified) =
        schema_classification(&state).await.map_err(internal)?;
    Ok(Json(ScanResponse {
        run_id: Uuid::new_v4(),
        status: "completed",
        fields: cs,
        totals: t,
        approved_plaintext: if req.include_approved_plaintext {
            approved
        } else {
            Default::default()
        },
        excluded,
        unclassified,
    }))
}

fn envelope(
    key: &[u8; 32],
    key_id: &str,
    f: &Field,
    id: Uuid,
    plain: &str,
) -> anyhow::Result<String> {
    database::field_encryption::encrypt_with_key_id(key, key_id, f.table, f.column, id, plain)
}

// Job lifecycle and authenticated envelope helpers.

fn decrypt_envelope(
    key: &[u8; 32],
    key_id: &str,
    f: &Field,
    id: Uuid,
    encoded: &str,
) -> anyhow::Result<String> {
    database::field_encryption::decrypt_with_key_id(key, key_id, f.table, f.column, id, encoded)
}

pub async fn create_job(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<JobResponse>), (StatusCode, Json<Value>)> {
    if !matches!(req.mode, Mode::Verify)
        && req.scope.tables.is_empty()
        && req.scope.fields.is_empty()
    {
        return Err(bad(
            "an explicit scope is required for database encryption jobs",
        ));
    }
    if state
        .pool
        .status()
        .is_some_and(|status| status.max_size < 2)
    {
        return Err(bad(
            "database encryption jobs require a pool with at least two connections",
        ));
    }
    if matches!(req.mode, Mode::Execute) && !state.pool.encryption_write_enabled() {
        return Err(bad("execute jobs require DB_ENCRYPTION_WRITE_ENABLED=true"));
    }
    if !(1..=1000).contains(&req.batch_size) {
        return Err(bad("batch_size must be between 1 and 1000"));
    }
    let expected_action = if matches!(req.mode, Mode::Verify) {
        "verify"
    } else {
        "encrypt"
    };
    if req.actions.as_slice() != [expected_action] {
        return Err(bad("actions do not match the requested job mode"));
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
        Mode::Verify => "verify",
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
    {
        let mut active = state.active_jobs.lock().unwrap_or_else(|e| e.into_inner());
        if !active.insert(id) {
            return;
        }
    }
    tokio::spawn(async move {
        let worker_state = state.clone();
        let result = tokio::spawn(async move { run_job(&worker_state, id).await }).await;
        let error_class = match result {
            Ok(Ok(())) => {
                state
                    .active_jobs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return;
            }
            Ok(Err(error)) => classify_job_error(&error),
            Err(_) => "worker_panicked",
        };
        {
            if let Ok(client) = state.pool.get().await {
                let _ = client
                    .execute(
                        "UPDATE database_encryption_jobs SET status='failed',last_error_class=$2,last_error_message=$2,progress=progress || jsonb_build_object('failure',jsonb_build_object('class',$2,'cursor',cursor)),completed_at=NOW() WHERE id=$1 AND status IN ('queued','running')",
                        &[&id, &error_class],
                    )
                    .await;
            }
            tracing::error!(
                job_id = %id,
                error_class,
                "Database encryption job failed"
            );
        }
        state
            .active_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    });
}

fn classify_job_error(error: &anyhow::Error) -> &'static str {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
    }) {
        "db_timeout"
    } else if error.to_string().contains("Pool") || error.to_string().contains("pool") {
        "pool_unavailable"
    } else if error.to_string().contains("encryption failed") {
        "encrypt_failed"
    } else if error.to_string().contains("invalid encrypted envelope") {
        "invalid_envelope"
    } else {
        "worker_failed"
    }
}

const GLOBAL_WORKER_LOCK_KEY: i64 = 0x4e454152444245;

async fn run_job(state: &DatabaseEncryptionState, id: Uuid) -> anyhow::Result<()> {
    let _worker_permit = state
        .worker_permit
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("database encryption worker stopped"))?;
    let mut client = state.pool.get().await?;
    run_locked_job(state, id, &mut client).await
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
    let mut plaintext = progress["plaintext"].as_i64().unwrap_or(0);
    let mut verified = progress["verified"].as_i64().unwrap_or(0);
    let mut invalid_envelopes = progress["invalid_envelopes"].as_i64().unwrap_or(0);
    let mut invalid_envelope_rows = progress["invalid_envelope_rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut plaintext_rows = progress["plaintext_rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    while field_index < fields.len() && max.is_none_or(|limit| processed < limit) {
        let field = fields[field_index];
        let cap = max
            .map(|limit| (limit - processed).min(batch))
            .unwrap_or(batch);
        let transaction = client.transaction().await?;
        transaction
            .batch_execute("SET LOCAL statement_timeout = '30s'")
            .await?;
        let locked: bool = transaction
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1)",
                &[&GLOBAL_WORKER_LOCK_KEY],
            )
            .await?
            .get(0);
        if !locked {
            transaction.rollback().await?;
            return Ok(());
        }
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
            "WITH candidates AS (\
                 SELECT id,{0}::text AS value FROM {1} \
                 WHERE id>$1 AND {0} IS NOT NULL ORDER BY id LIMIT $2{locking_clause}\
             ), sized AS (\
                 SELECT id,value,\
                    SUM(octet_length(value)) OVER (ORDER BY id) AS cumulative_bytes,\
                    ROW_NUMBER() OVER (ORDER BY id) AS row_number \
                 FROM candidates\
             ) \
             SELECT id,value FROM sized \
             WHERE cumulative_bytes<=$3 OR row_number=1 ORDER BY id",
            field.column, field.table
        );
        let rows = transaction
            .query(&query, &[&after_id, &cap, &MAX_BATCH_PLAINTEXT_BYTES])
            .await?;
        if rows.is_empty() {
            field_index += 1;
            after_id = Uuid::nil();
        } else {
            let mut row_ids = Vec::with_capacity(rows.len());
            let mut encrypted_values = Vec::with_capacity(rows.len());
            for row in &rows {
                let row_id: Uuid = row.get(0);
                after_id = row_id;
                if mode == "verify" {
                    let raw: String = row.get(1);
                    match serde_json::from_str::<Value>(&raw) {
                        Ok(value) if value[MARKER] == true => {
                            if database::field_encryption::is_envelope(&value)
                                && decrypt_envelope(&state.key, &state.key_id, field, row_id, &raw)
                                    .is_ok()
                            {
                                verified += 1;
                                continue;
                            }
                            invalid_envelopes += 1;
                            if invalid_envelope_rows.len() < 100 {
                                invalid_envelope_rows.push(json!({
                                    "table": field.table,
                                    "column": field.column,
                                    "id": row_id,
                                }));
                            }
                        }
                        _ => {
                            plaintext += 1;
                            if plaintext_rows.len() < 100 {
                                plaintext_rows.push(json!({
                                    "table": field.table,
                                    "column": field.column,
                                    "id": row_id,
                                }));
                            }
                        }
                    }
                    continue;
                }
                if mode == "execute" {
                    let plaintext: String = row.get(1);
                    if let Ok(value) = serde_json::from_str::<Value>(&plaintext) {
                        if database::field_encryption::is_envelope(&value) {
                            if decrypt_envelope(
                                &state.key,
                                &state.key_id,
                                field,
                                row_id,
                                &plaintext,
                            )
                            .is_err()
                            {
                                invalid_envelopes += 1;
                                if invalid_envelope_rows.len() < 100 {
                                    invalid_envelope_rows.push(json!({
                                        "table": field.table,
                                        "column": field.column,
                                        "id": row_id,
                                    }));
                                }
                            }
                            continue;
                        }
                    }
                    row_ids.push(row_id);
                    encrypted_values.push(envelope(
                        &state.key,
                        &state.key_id,
                        field,
                        row_id,
                        &plaintext,
                    )?);
                }
            }
            if mode == "execute" && !row_ids.is_empty() {
                if field.table == "responses" && field.column == "metadata" {
                    transaction
                        .execute(
                            "UPDATE responses SET is_root_response=TRUE WHERE id=ANY($1) AND metadata->>'root_response'='true'",
                            &[&row_ids],
                        )
                        .await?;
                }
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
                    &json!({
                        "processed": processed,
                        "encrypted": encrypted,
                        "verified": verified,
                        "plaintext": plaintext,
                        "invalid_envelopes": invalid_envelopes,
                        "invalid_envelope_rows": invalid_envelope_rows,
                        "plaintext_rows": plaintext_rows,
                    }),
                    &json!({
                        "field_index": field_index,
                        "table": field.table,
                        "column": field.column,
                        "after_id": after_id,
                    }),
                ],
            )
            .await?;
        transaction.commit().await?;
    }

    if mode == "verify" {
        let (approved, excluded, unclassified) = classify_schema(client).await?;
        let pass = plaintext == 0 && invalid_envelopes == 0 && unclassified.is_empty();
        client
            .execute(
                "UPDATE database_encryption_jobs SET status='completed',completed_at=NOW(),progress=progress || $2 WHERE id=$1",
                &[&id, &json!({
                    "pass": pass,
                    "approved_plaintext": approved,
                    "excluded": excluded,
                    "unclassified": unclassified,
                })],
            )
            .await?;
    } else {
        client
            .execute(
                "UPDATE database_encryption_jobs SET status='completed',completed_at=NOW() WHERE id=$1",
                &[&id],
            )
            .await?;
    }
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
    drop(c);
    get_inner(&state, id).await
}

#[derive(Debug, Deserialize, Default)]
pub struct VerifyRequest {
    #[serde(default)]
    scope: Scope,
    #[serde(default = "yes")]
    fail_on_approved_plaintext_without_reason: bool,
    batch_size: Option<i64>,
}

fn yes() -> bool {
    true
}

pub async fn verify(
    Extension(state): Extension<DatabaseEncryptionState>,
    Extension(admin): Extension<AdminUser>,
    Json(req): Json<VerifyRequest>,
) -> Result<(StatusCode, Json<JobResponse>), (StatusCode, Json<Value>)> {
    if !req.fail_on_approved_plaintext_without_reason {
        return Err(bad(
            "verification cannot ignore unclassified plaintext columns",
        ));
    }
    create_job(
        Extension(state),
        Extension(admin),
        Json(CreateJobRequest {
            mode: Mode::Verify,
            scope: req.scope,
            batch_size: req.batch_size.unwrap_or_else(batch_default),
            max_rows: None,
            actions: vec!["verify".to_string()],
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_hides_plaintext() {
        let v = envelope(&[7; 32], "test-v1", &FIELDS[0], Uuid::nil(), "secret").unwrap();
        assert!(v.contains(MARKER));
        assert!(!v.contains("secret"));
        assert_eq!(
            decrypt_envelope(&[7; 32], "test-v1", &FIELDS[0], Uuid::nil(), &v).unwrap(),
            "secret"
        );
    }
    #[test]
    fn envelope_authenticates_context_and_key() {
        let v = envelope(&[7; 32], "test-v1", &FIELDS[0], Uuid::nil(), "secret").unwrap();
        assert!(decrypt_envelope(&[8; 32], "test-v1", &FIELDS[0], Uuid::nil(), &v).is_err());
        assert!(decrypt_envelope(&[7; 32], "test-v1", &FIELDS[1], Uuid::nil(), &v).is_err());
        assert!(decrypt_envelope(&[7; 32], "test-v1", &FIELDS[0], Uuid::new_v4(), &v).is_err());
        assert!(decrypt_envelope(&[7; 32], "test-v2", &FIELDS[0], Uuid::nil(), &v).is_err());
    }

    #[test]
    fn malformed_envelope_is_rejected() {
        let malformed = json!({MARKER: true, "version": 1, "alg": "AES-256-GCM"}).to_string();
        assert!(
            decrypt_envelope(&[7; 32], "test-v1", &FIELDS[0], Uuid::nil(), &malformed).is_err()
        );
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

        let mut approved = APPROVED
            .iter()
            .flat_map(|group| {
                assert!(!group.reason.trim().is_empty());
                group
                    .columns
                    .iter()
                    .map(move |column| (group.table, *column))
            })
            .collect::<Vec<_>>();
        let approved_len = approved.len();
        approved.sort();
        approved.dedup();
        assert_eq!(approved.len(), approved_len);
        assert!(approved
            .iter()
            .all(|(table, column)| !is_encrypted_field(table, column)));

        assert!(EXCLUDED_TABLES
            .iter()
            .all(|excluded| !excluded.reason.trim().is_empty()));
        assert!(EXCLUDED_TABLES.iter().all(|excluded| {
            !FIELDS.iter().any(|field| field.table == excluded.table)
                && APPROVED.iter().all(|group| group.table != excluded.table)
        }));
    }

    #[test]
    fn schema_exclusion_is_exactly_scoped_to_postgres_log() {
        assert!(excluded_table_reason("postgres_log").is_some());
        assert!(excluded_table_reason("postgres_logs").is_none());
        assert!(excluded_table_reason("postgres_log_archive").is_none());
        assert!(excluded_table_reason("postgres_user_tokens").is_none());
    }

    #[test]
    fn worker_errors_are_reduced_to_safe_classes() {
        assert_eq!(
            classify_job_error(&anyhow::anyhow!("database pool unavailable")),
            "pool_unavailable"
        );
        assert_eq!(
            classify_job_error(&anyhow::anyhow!("encryption failed: secret details")),
            "encrypt_failed"
        );
        assert_eq!(
            classify_job_error(&anyhow::anyhow!(
                "invalid encrypted envelope: secret details"
            )),
            "invalid_envelope"
        );
        assert_eq!(
            classify_job_error(&anyhow::anyhow!("unexpected secret details")),
            "worker_failed"
        );
    }
}
