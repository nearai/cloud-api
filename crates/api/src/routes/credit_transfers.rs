use crate::middleware::auth::{auth_middleware, AdminUser, AuthState, AuthenticatedUser};
use crate::models::ErrorResponse;
use axum::{
    extract::{Extension, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use database::repositories::credit_transfers::{
    hash_credential, CreditTransferRepository, OrganizationPolicy, TransferActor, TransferError,
    TransferHistoryOptions, TransferRequest,
};
use database::repositories::organization_credentials::{
    CredentialRequest, OrganizationCredentialRepository,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn record_outcome(outcome: &'static str) {
    use opentelemetry::{global, KeyValue};
    use std::sync::LazyLock;

    static COUNTER: LazyLock<opentelemetry::metrics::Counter<u64>> = LazyLock::new(|| {
        global::meter("credit_transfers")
            .u64_counter("credit_transfer_operations")
            .with_description("Credit transfer outcomes; no customer labels")
            .build()
    });
    COUNTER.add(1, &[KeyValue::new("outcome", outcome)]);
}

fn error(error: anyhow::Error) -> ApiError {
    let database_busy = error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(|error| error.code())
            .is_some_and(|code| matches!(code.code(), "55P03" | "57014" | "40P01"))
    });
    let code = error
        .downcast_ref::<TransferError>()
        .map(|e| e.0)
        .unwrap_or(if database_busy {
            "accounting_busy"
        } else {
            "transfer_unavailable"
        });
    record_outcome(code);
    let status = match code {
        "credential_refused" | "sender_not_enabled" | "credential_management_disabled" => {
            StatusCode::FORBIDDEN
        }
        "organization_unavailable" | "credential_not_found" => StatusCode::NOT_FOUND,
        "insufficient_posted_capacity"
        | "sender_not_reconciled"
        | "idempotency_conflict"
        | "capacity_overflow"
        | "credential_transfer_limit_exceeded"
        | "organization_transfer_limit_exceeded" => StatusCode::CONFLICT,
        "transfer_rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        "transfer_unavailable" | "accounting_busy" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    // Never log arbitrary terms, metadata, token hashes, or database bind values.
    tracing::info!(outcome = code, "Credit transfer operation rejected");
    (status, Json(ErrorResponse::new(code.into(), code.into())))
}

async fn authenticate(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer oc_"));
    if let Some(token) = token {
        if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(error(TransferError("credential_refused").into()));
        }
        let actor = TransferActor::Credential(hash_credential(&format!("oc_{token}")));
        request.extensions_mut().insert(actor);
        return Ok(next.run(request).await);
    }
    auth_middleware(State(state), request, next).await
}

fn actor(
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
) -> Result<TransferActor, ApiError> {
    token
        .map(|Extension(actor)| actor)
        .or_else(|| user.map(|Extension(user)| TransferActor::Owner(user.0.id)))
        .ok_or_else(|| error(TransferError("credential_refused").into()))
}

pub fn routes(
    repository: CreditTransferRepository,
    credentials: OrganizationCredentialRepository,
    auth: AuthState,
) -> Router {
    Router::new()
        .route("/organizations/{org_id}/credit/transfer", post(create))
        .route("/organizations/{org_id}/credit/transfers", get(history))
        .route("/organizations/{org_id}/credit/transfers/{id}", get(detail))
        .route(
            "/organizations/{org_id}/credit/transfer/capacity",
            get(capacity),
        )
        .route(
            "/organizations/{org_id}/credentials",
            post(create_credential).get(list_credentials),
        )
        .route(
            "/organizations/{org_id}/credentials/{id}",
            delete(revoke_credential).put(update_credential),
        )
        .route(
            "/organizations/{org_id}/credentials/{id}/rotate",
            post(rotate_credential),
        )
        .layer(Extension(credentials))
        .layer(Extension(repository))
        .layer(from_fn_with_state(auth, authenticate))
}

/// Single source transfer. Memo and metadata are visible to both parties.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTransferRequest {
    pub recipient_organization_id: Uuid,
    /// payment, staking_farm, or grant (grant requires explicit policy opt-in).
    pub source_credit_type: String,
    /// Positive nano-USD amount; mutually exclusive with recipient_cumulative.
    pub amount: Option<i64>,
    /// Nonnegative target for this sender, recipient, and payout program.
    pub recipient_cumulative: Option<i64>,
    /// Required for cumulative targets; optional for amount sends.
    pub payout_program: Option<String>,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub metadata: serde_json::Map<String, Value>,
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credit/transfer",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Sender organization"),
        ("Idempotency-Key" = Option<String>, Header, description = "Generate a UUID once in the client before sending and reuse it on retries; required for amount sends, prohibited for cumulative targets; 1–100 characters")
    ),
    request_body = CreateTransferRequest,
    responses(
        (status = 201, description = "Delivered; includes sender accounting only", body = Value),
        (status = 200, description = "Original committed response replay or cumulative target already reached", body = Value),
        (status = 409, description = "idempotency_conflict, insufficient_posted_capacity or sender_not_reconciled", body = ErrorResponse)
    ),
    security(("session_token" = []), ("organization_credential" = []))
)]
pub async fn create(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    request: Result<Json<CreateTransferRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(request) =
        request.map_err(|_| error(TransferError("invalid_transfer_terms").into()))?;
    let request = TransferRequest {
        recipient_organization_id: request.recipient_organization_id,
        source_credit_type: request.source_credit_type,
        amount: request.amount,
        recipient_cumulative: request.recipient_cumulative,
        payout_program: request.payout_program,
        memo: request.memo,
        metadata: request.metadata,
    };
    let actor = actor(user, token)?;
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (result, replay) = repository
        .transfer(org, &actor, key, &request)
        .await
        .map_err(error)?;
    record_outcome(if result["no_op"] == true {
        "no_op"
    } else if replay {
        "replay"
    } else {
        "created"
    });
    tracing::info!(replay, "Credit transfer completed");
    Ok((
        if replay {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(result),
    ))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    idempotency_key: Option<String>,
    before_time: Option<DateTime<Utc>>,
    before_id: Option<Uuid>,
    limit: Option<i64>,
    direction: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/credit/transfers",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization"),
        ("idempotency_key" = Option<String>, Query, description = "Outgoing lookup"),
        ("before_time" = Option<String>, Query, description = "Cursor timestamp"),
        ("before_id" = Option<Uuid>, Query, description = "Cursor ID; required with before_time"),
        ("limit" = Option<i64>, Query, description = "Page size 1–100"),
        ("direction" = Option<String>, Query, description = "incoming or outgoing; omit for both")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []), ("organization_credential" = []))
)]
pub async fn history(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path(org): Path<Uuid>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.before_time.is_some() != query.before_id.is_some() {
        return Err(error(TransferError("invalid_cursor").into()));
    }
    repository
        .history(
            org,
            &actor(user, token)?,
            None,
            query.idempotency_key.as_deref(),
            query.before_time.zip(query.before_id),
            TransferHistoryOptions {
                limit: query.limit.unwrap_or(50),
                direction: query.direction,
            },
        )
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/credit/transfers/{id}",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization"),
        ("id" = Uuid, Path, description = "Record ID")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []), ("organization_credential" = []))
)]
pub async fn detail(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let result = repository
        .history(org, &actor(user, token)?, Some(id), None, None, 1)
        .await
        .map_err(error)?;
    let item = result["transfers"]
        .as_array()
        .and_then(|items| items.first())
        .ok_or_else(|| error(TransferError("organization_unavailable").into()))?;
    Ok(Json(item.clone()))
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/credit/transfer/capacity",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []), ("organization_credential" = []))
)]
pub async fn capacity(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    repository
        .capacity(org, &actor(user, token)?)
        .await
        .map(Json)
        .map_err(error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRequest {
    policy: OrganizationPolicy,
    reason: String,
}

#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/policy",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn get_policy(
    Extension(repository): Extension<CreditTransferRepository>,
    Extension(_admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    repository.policy(org, None).await.map(Json).map_err(error)
}

#[utoipa::path(
    put,
    path = "/v1/admin/organizations/{org_id}/policy",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = PolicyRequestSchema,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn put_policy(
    Extension(repository): Extension<CreditTransferRepository>,
    Extension(admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
    request: Result<Json<PolicyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) =
        request.map_err(|_| error(TransferError("invalid_policy_configuration").into()))?;
    repository
        .configure_policy(org, Some((request.policy, admin.0.id, &request.reason)))
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credentials",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = CredentialRequestSchema,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn create_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(org): Path<Uuid>,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .save(org, user.0 .0.id, None, &request)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    delete,
    path = "/v1/organizations/{org_id}/credentials/{id}",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization"),
        ("id" = Uuid, Path, description = "Record ID")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn revoke_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .revoke(org, user.0 .0.id, id)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/credentials",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization")),
    responses((status = 200, description = "Credential configurations, lifetime usage and audit; no secrets", body = Value)),
    security(("session_token" = []))
)]
pub async fn list_credentials(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .list(org, user.0 .0.id)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    put,
    path = "/v1/organizations/{org_id}/credentials/{id}",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization"), ("id" = Uuid, Path, description = "Credential")),
    request_body = CredentialRequestSchema,
    responses((status = 200, description = "Replace configuration; lifetime usage is preserved", body = Value)),
    security(("session_token" = []))
)]
pub async fn update_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .save(org, user.0 .0.id, Some(id), &request)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credentials/{id}/rotate",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization"), ("id" = Uuid, Path, description = "Credential")),
    responses((status = 200, description = "New secret shown once; old secret immediately invalid; budget unchanged", body = Value)),
    security(("session_token" = []))
)]
pub async fn rotate_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .rotate(org, user.0 .0.id, id)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/credit/transfer/reconciliation",
    tag = "Credits",
    params(("org_id" = Uuid, Path, description = "Organization")),
    responses((status = 200, description = "Read-only consistency and backing checks; no repair", body = Value)),
    security(("session_token" = []))
)]
pub async fn reconciliation(
    Extension(repository): Extension<CreditTransferRepository>,
    Extension(_admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let result = repository.reconciliation(org).await.map_err(error)?;
    if result["organization_exists"] != true {
        return Err(error(TransferError("organization_unavailable").into()));
    }
    Ok(Json(result))
}

// Keep wire schemas explicit even though repository DTOs own validation.
#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct TransferConstraintsSchema {
    /// Integer nano-USD. Omitted/null means no additional ceiling; zero blocks sends.
    #[schema(minimum = 0)]
    max_amount_per_transfer: Option<i64>,
    /// Lifetime nano-USD budget. Rotation and configuration updates never reset usage.
    #[schema(minimum = 0)]
    max_total_amount: Option<i64>,
    /// Policy default: payment and staking_farm. Grant requires explicit policy opt-in.
    /// Credential omission adds no restriction; an empty list permits no sources.
    allowed_source_types: Option<Vec<String>>,
    /// Omitted/null permits any recipient; an empty list permits none. At most 1,000 IDs.
    allowed_recipient_ids: Option<Vec<Uuid>>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialConstraintsSchema {
    #[schema(rename = "credits:transfer")]
    transfer: Option<TransferConstraintsSchema>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialRequestSchema {
    /// Nonempty display name, at most 200 UTF-8 bytes.
    name: String,
    /// Explicit supported permissions: credits:read and/or credits:transfer.
    permissions: Vec<String>,
    constraints: Option<CredentialConstraintsSchema>,
    /// Future timestamp no more than 366 days away.
    expires_at: DateTime<Utc>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct TransferPolicySchema {
    enabled: bool,
    #[schema(minimum = 1, maximum = 100000, default = 60)]
    max_sends_per_minute: Option<i64>,
    constraints: Option<TransferConstraintsSchema>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct OrganizationPolicySchema {
    credentials: CredentialPolicySchema,
    credits_transfer: TransferPolicySchema,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct PolicyRequestSchema {
    policy: OrganizationPolicySchema,
    /// Required audit reason, at most 2,000 UTF-8 bytes.
    reason: String,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialPolicySchema {
    /// Gate owner creation, configuration updates and rotation. Listing/revocation remain available.
    enabled: bool,
}
