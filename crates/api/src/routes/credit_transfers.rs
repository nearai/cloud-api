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
    hash_credential, CreditTransferRepository, TransferActor, TransferError,
    TransferHistoryOptions, TransferRequest,
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
        "credential_refused" | "sender_not_enabled" => StatusCode::FORBIDDEN,
        "organization_unavailable" => StatusCode::NOT_FOUND,
        "insufficient_posted_capacity"
        | "sender_not_reconciled"
        | "idempotency_conflict"
        | "capacity_overflow" => StatusCode::CONFLICT,
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
        .and_then(|h| h.strip_prefix("Bearer ct_"));
    if let Some(token) = token {
        if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(error(TransferError("credential_refused").into()));
        }
        let actor = TransferActor::Credential(hash_credential(&format!("ct_{token}")));
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

pub fn routes(repository: CreditTransferRepository, auth: AuthState) -> Router {
    Router::new()
        .route("/organizations/{org_id}/credit/transfer", post(create))
        .route("/organizations/{org_id}/credit/transfers", get(history))
        .route("/organizations/{org_id}/credit/transfers/{id}", get(detail))
        .route(
            "/organizations/{org_id}/credit/transfer-capacity",
            get(capacity),
        )
        .route(
            "/organizations/{org_id}/credit/transfer-credentials",
            post(create_credential),
        )
        .route(
            "/organizations/{org_id}/credit/transfer-credentials/{id}",
            delete(revoke_credential),
        )
        .layer(Extension(repository))
        .layer(from_fn_with_state(auth, authenticate))
}

/// Single source transfer. Memo and metadata are visible to both parties.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTransferRequest {
    pub recipient_organization_id: Uuid,
    /// Only payment or staking_farm; integer nano-USD amount must be positive.
    pub source_credit_type: String,
    pub amount: i64,
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
        ("Idempotency-Key" = String, Header, description = "Stable key for this intended transfer; required, 1–100 characters")
    ),
    request_body = CreateTransferRequest,
    responses(
        (status = 201, description = "Delivered; includes sender accounting only", body = Value),
        (status = 200, description = "Original committed response replay", body = Value),
        (status = 409, description = "idempotency_conflict, insufficient_posted_capacity or sender_not_reconciled", body = ErrorResponse)
    ),
    security(("session_token" = []), ("transfer_credential" = []))
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
    record_outcome(if replay { "replay" } else { "created" });
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
    security(("session_token" = []), ("transfer_credential" = []))
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
    security(("session_token" = []), ("transfer_credential" = []))
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
    path = "/v1/organizations/{org_id}/credit/transfer-capacity",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []), ("transfer_credential" = []))
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PolicyRequest {
    enabled: bool,
    reason: String,
}

#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/credit/transfer-policy",
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
    path = "/v1/admin/organizations/{org_id}/credit/transfer-policy",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = PolicyRequest,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn put_policy(
    Extension(repository): Extension<CreditTransferRepository>,
    Extension(admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
    Json(request): Json<PolicyRequest>,
) -> Result<Json<Value>, ApiError> {
    repository
        .policy(org, Some((request.enabled, admin.0.id, &request.reason)))
        .await
        .map(Json)
        .map_err(error)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CredentialRequest {
    expires_at: DateTime<Utc>,
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credit/transfer-credentials",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = CredentialRequest,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn create_credential(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(org): Path<Uuid>,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .credential(org, user.0 .0.id, None, Some(request.expires_at))
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    delete,
    path = "/v1/organizations/{org_id}/credit/transfer-credentials/{id}",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization"),
        ("id" = Uuid, Path, description = "Record ID")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn revoke_credential(
    Extension(repository): Extension<CreditTransferRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let user = user.ok_or_else(|| error(TransferError("credential_refused").into()))?;
    repository
        .credential(org, user.0 .0.id, Some(id), None)
        .await
        .map(Json)
        .map_err(error)
}
