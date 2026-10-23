use crate::middleware::auth::{AdminUser, AuthState, AuthenticatedUser};
use crate::middleware::organization_credentials::{
    actor, authenticate, error as operation_error, error_code, ApiError,
};
use crate::models::ErrorResponse;
use axum::{
    extract::{Extension, Path, Query},
    http::{HeaderMap, StatusCode},
    middleware::from_fn_with_state,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use services::organization_credits::models::{
    HistoryRequest, TransferActor, TransferError, TransferHistoryOptions, TransferRequest,
};
use services::organization_credits::OrganizationCreditServiceTrait;
use std::sync::Arc;
use uuid::Uuid;

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
    record_outcome(error_code(&error));
    operation_error(error)
}

pub fn routes(service: Arc<dyn OrganizationCreditServiceTrait>, auth: AuthState) -> Router {
    Router::new()
        .route("/organizations/{org_id}/credit/transfer", post(create))
        .route("/organizations/{org_id}/credit/transfers", get(history))
        .route("/organizations/{org_id}/credit/transfers/{id}", get(detail))
        .route(
            "/organizations/{org_id}/credit/transfer/capacity",
            get(capacity),
        )
        .layer(Extension(service))
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
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
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
    let (result, replay) = service
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
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path(org): Path<Uuid>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.before_time.is_some() != query.before_id.is_some() {
        return Err(error(TransferError("invalid_cursor").into()));
    }
    service
        .history(
            org,
            &actor(user, token)?,
            HistoryRequest {
                id: None,
                key: query.idempotency_key,
                before: query.before_time.zip(query.before_id),
                options: TransferHistoryOptions {
                    limit: query.limit.unwrap_or(50),
                    direction: query.direction,
                },
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
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let result = service
        .history(
            org,
            &actor(user, token)?,
            HistoryRequest {
                id: Some(id),
                key: None,
                before: None,
                options: 1.into(),
            },
        )
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
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<TransferActor>>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    service
        .capacity(org, &actor(user, token)?)
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
    Extension(service): Extension<Arc<dyn OrganizationCreditServiceTrait>>,
    Extension(_admin): Extension<AdminUser>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let result = service.reconciliation(org).await.map_err(error)?;
    if result["organization_exists"] != true {
        return Err(error(TransferError("organization_unavailable").into()));
    }
    Ok(Json(result))
}
